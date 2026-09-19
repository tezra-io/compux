#!/usr/bin/env perl
# Test-only fake compux sidecar: line-framed JSON, no native code. It exists so
# `Compux.Transport` can be driven against a REAL Port (fragments, interleaving,
# a process that exits) without the Rust binary, the network, or host state.
#
# It is FAITHFUL on the points a library regression could otherwise hide, each
# mirroring the real sidecar (`native/compux/src/wire.rs`, `control.rs`, `gate.rs`):
#
#   * a mutating success carries a receipt, derived the way `Receipt::derive` does
#     it — `dispatch: "sent"`, and `effect: "not_observed"` only when the check
#     really brought evidence back, else `unknown`. Whether a request mutates is
#     read off the line's own `mutation_seq`, which is exactly the sidecar's rule;
#   * CHECKS (protocol 10): a successful action that dispatches input ALWAYS carries
#     `check` in its receipt — `{"kind":"none"}` when none was asked for — because a
#     fake that could leave it out would hide a consumer that stopped reading it.
#     This one settles instantly (`settle: "stable"`, `changed: false`) and reports
#     timings of zero, and an `image` check really does answer an image, so the
#     `not_observed` effect and the `observation_id_after` that ride one are the
#     helper's own shapes rather than convenient ones;
#   * a TYPELESS line is the protocol-6 shape and there is no legacy mode, so it
#     is refused. Naming an action it is answered with an `ack` carrying THIS
#     protocol version (`wire::untagged` -> `Reply::CaptureAck`); naming none it
#     gets silence;
#   * `hello` answers through the same envelope as every other response, so it
#     carries `session_generation` as well as `sidecar_generation`;
#   * `authorization_generation` COUNTS, one per control, from the gate's initial 1;
#   * ADDRESSING (protocol 8) behaves as `wire::addressing` does: a `screenshot`
#     mints an observation and names it on the reply, a pointer action or `inspect`
#     without an `observation_id` is `observation_required`, one carrying a `region`
#     is `unknown_field`, an id nobody minted is `unknown_observation`, and a point
#     outside the minted image is `point_outside_observation`. Each refusal carries
#     the `not_sent` receipt the real helper sends, because a library regression
#     that dropped a refusal's receipt would otherwise look like a success;
#   * REFERENCES (protocol 9): `elements` mints an observation and hands back one
#     element carrying an `element_ref`, and `press` / `set_value` / a pointer
#     action addressed by that reference resolve it the way the helper does —
#     `element_required` with none, `stale_element` for one nobody minted,
#     `addressing_conflict` for a point and a reference on the same request. The
#     REPLIES have the helper's own shape, not a convenient one: `press` answers a
#     bare `{"ok":true}` with an `ax` receipt, and `set_value` answers `verified`
#     plus the value it read back, with the `verified` effect that read-back earned
#     — so a library regression that mis-read either is caught here.
#
# Autoflush ($| = 1) so each reply reaches the Port immediately (a block-buffered
# write to a pipe would hang the reader).
#
# Request actions:
#   hello         -> the identity handshake (protocol 10, a boot generation)
#   screenshot    -> a 100x50 image that mints an observation and names it
#   elements      -> one element (`e1`) in a fresh observation, as the helper lists it
#   left_click, right_click, double_click, mouse_move, left_click_drag, scroll,
#   inspect       -> addressed: they resolve their observation or refuse
#   press, set_value -> addressed by element_ref inside an observation
#   boom          -> exit 7                     (a sidecar death mid-request)
#   hang          -> sleep, never reply         (the action deadline)
#   defer         -> announce a session_event, then hold the response back
#   late          -> reply after half a second  (a response past its deadline)
#   flush_exit    -> reply, THEN exit 75        (the capture-stall fail-fast: an
#                                                IDLE exit, which no reply carries)
#   stash         -> remember the id, reply nothing (a request left to time out)
#   flush_stash   -> answer every stashed id, then this one
#   capture       -> ok with "data", so its receipt earns effect not_observed
#   no_receipt    -> a mutating success with NO receipt (a sidecar-side fault)
#   echo          -> reply naming the envelope fields the request carried
#   dribble       -> 10 unterminated 64-byte chunks 250 ms apart, then sleep
#   oversize      -> one response padded to "bytes" total, split by the line limit
#   unknown_id    -> a well-formed response naming a request nobody sent
#   future_id     -> a response naming an id ABOVE anything yet minted
#   stale_boot    -> a response from a different sidecar generation
#   stale_session -> a response from a different session generation
#   malformed     -> a line that is not JSON
#   history_event -> an unsolicited computer-history `event` frame
#   history_ack   -> an unsolicited computer-history `ack` frame
#   refuse        -> ok:false + error + receipt (a refused mutation)
#   anything else -> {"ok":true,"pong":true}
#
# Control frames are answered per FAKE_CONTROL_MODE: "ack" (default) replies with
# a control_ack and then releases any deferred response, "silent" never answers,
# "exit" ends the process with status 9.
#
# Env: FAKE_PROTOCOL_VERSION, FAKE_SIDECAR_GENERATION, FAKE_CONTROL_MODE.
use strict;
use warnings;
$| = 1;

my $PROTOCOL     = defined $ENV{FAKE_PROTOCOL_VERSION}   ? $ENV{FAKE_PROTOCOL_VERSION}   : 10;
my $BOOT         = defined $ENV{FAKE_SIDECAR_GENERATION} ? $ENV{FAKE_SIDECAR_GENERATION} : 'boot-test';
my $CONTROL_MODE = defined $ENV{FAKE_CONTROL_MODE}       ? $ENV{FAKE_CONTROL_MODE}       : 'ack';

# The gate's own initial value; every control bumps it.
my $AUTH = 1;

my $deferred;
my @stashed;

# The observation table, as small as the real one needs it to be here: the ids this
# process has minted, the size of the image each names, and — for the one an
# `elements` reply mints — the single reference it listed.
my $OBS_COUNTER = 0;
my %OBSERVED;
my %ELEMENTS;
my $SENT_W = 100;
my $SENT_H = 50;

# `wire::ADDRESSED_ACTIONS`: the actions addressed INTO an observation, which take
# no rectangle of their own.
my %ADDRESSED = map { $_ => 1 } qw(
    left_click right_click double_click mouse_move left_click_drag scroll inspect
    press set_value
);

# `wire::ELEMENT_ACTIONS`: addressed by a control and never by a point.
my %ELEMENT_ONLY = map { $_ => 1 } qw(press set_value);

while (my $line = <STDIN>) {
    my ($type)   = $line =~ /"type":"([^"]*)"/;
    my ($id)     = $line =~ /"request_id":"([^"]*)"/;
    my ($action) = $line =~ /"action":"([^"]*)"/;
    $id     = ''  unless defined $id;
    $action = ''  unless defined $action;

    if    (!defined $type)       { untagged($action); }
    elsif ($type eq 'control')   { control($id, $action); }
    elsif ($type eq 'request')   { request($id, $action, $line); }
    else                         { }  # an unknown tag: stderr is the whole report
}

sub envelope {
    my ($id, $boot, $session) = @_;
    return qq("request_id":"$id","sidecar_generation":"$boot","session_generation":$session);
}

# The check a request asked for, as the receipt reports it. Absent means `none`:
# the helper always says which evidence it carries, and a fake that left the field
# off would hide a consumer that stopped reading it. This one settles instantly, so
# a check that ran is always `stable`.
sub check_for {
    my ($line) = @_;
    my ($kind) = $line =~ /"check":"([^"]*)"/;
    return qq(,"check":{"kind":"none"}) unless defined $kind && $kind ne 'none';
    return qq(,"check":{"kind":"semantic"}) if $kind eq 'semantic';
    return qq(,"check":{"kind":"image","settle":"stable","changed":false});
}

# A mutating request earns a receipt, and only a check that really brought
# something back has evidence to assess.
sub receipt_for {
    my ($line, $has_evidence, $after) = @_;
    return '' unless $line =~ /"mutation_seq":\d+/;
    my $effect = $has_evidence ? 'not_observed' : 'unknown';
    my ($named) = $line =~ /"observation_id":"([^"]*)"/;
    my $before = defined $named ? qq(,"observation_id_before":"$named") : '';
    my $ids = defined $after ? qq($before,"observation_id_after":"$after") : $before;
    return qq(,"receipt":{"dispatch":"sent","effect":"$effect",)
        . qq("input_method":"foreground_hid","timings_ms":{"input":1,"settle":0,)
        . qq("capture":0,"encode":0}$ids) . check_for($line) . qq(});
}

sub ok_response {
    my ($id, $extra, $line, $has_evidence, $after) = @_;
    my $head = envelope($id, $BOOT, 1);
    my $receipt = defined $line ? receipt_for($line, $has_evidence, $after) : '';
    print qq({"type":"response",$head,"ok":true,$extra$receipt}\n);
}

# Protocol 6's untagged line. Refused, never served — and when it names an action
# it is refused in the `ack` family, the one vocabulary the client that still
# sends this shape can read, so it sees this sidecar's version rather than
# waiting out a handshake nothing will answer.
sub untagged {
    my ($action) = @_;
    return if $action eq '';
    print qq({"type":"ack","action":"$action","ok":false,"protocol_version":$PROTOCOL,)
        . qq("error":"this wire needs a tagged frame; this line has no type"}\n);
}

sub control {
    my ($id, $action) = @_;
    exit 9 if $CONTROL_MODE eq 'exit';
    return if $CONTROL_MODE eq 'silent';

    $AUTH++;
    my $in_flight = defined $deferred ? qq("$deferred") : 'null';
    my $head = envelope($id, $BOOT, 1);
    print qq({"type":"control_ack",$head,"action":"$action","ok":true,)
        . qq("authorization_generation":$AUTH,"in_flight_request_id":$in_flight}\n);

    # The barrier is installed; the held request now answers as a cancelled one.
    if (defined $deferred) {
        my $head2 = envelope($deferred, $BOOT, 1);
        print qq({"type":"response",$head2,"ok":false,"error":"cancelled",)
            . qq("receipt":{"dispatch":"partial","effect":"unknown","input_method":"foreground_hid"}}\n);
        undef $deferred;
    }
}

# A refusal with the receipt the real helper sends: nothing was dispatched, and the
# id the caller named rides along so a reader can see what it was aiming at.
sub refuse {
    my ($id, $error, $detail, $line) = @_;
    my $head = envelope($id, $BOOT, 1);
    my ($named) = $line =~ /"observation_id":"([^"]*)"/;
    my $before = defined $named ? qq(,"observation_id_before":"$named") : '';
    print qq({"type":"response",$head,"ok":false,"error":"$error","detail":"$detail",)
        . qq("receipt":{"dispatch":"not_sent","effect":"unknown",)
        . qq("input_method":"foreground_hid","timings_ms":{"input":0,"settle":0,)
        . qq("capture":0,"encode":0}$before}}\n);
}

# Protocol 8/9: an addressed action names the reply its target was read from, and
# takes no rectangle. Answers the resolved observation's id, or undef once it has
# already written the refusal.
sub addressed {
    my ($id, $action, $line) = @_;

    if ($line =~ /"region":/) {
        refuse($id, 'unknown_field',
            "region is not accepted on $action", $line);
        return undef;
    }

    my ($named) = $line =~ /"observation_id":"([^"]*)"/;
    if (!defined $named || $named eq '') {
        refuse($id, 'observation_required',
            "$action needs observation_id", $line);
        return undef;
    }
    if (!exists $OBSERVED{$named}) {
        refuse($id, 'unknown_observation',
            'no image of that name is held', $line);
        return undef;
    }

    my ($reference) = $line =~ /"element_ref":"([^"]*)"/;
    my $has_point = $line =~ /"x":-?\d+/;

    # A point AND a control on one request: nothing may guess which was meant.
    if (defined $reference && $has_point) {
        refuse($id, 'addressing_conflict',
            "$action is addressed either by a point or by element_ref", $line);
        return undef;
    }
    if ($ELEMENT_ONLY{$action} && !defined $reference) {
        refuse($id, 'element_required',
            "$action needs element_ref", $line);
        return undef;
    }
    if (defined $reference) {
        return $named if exists $ELEMENTS{$named} && exists $ELEMENTS{$named}{$reference};
        refuse($id, 'stale_element',
            'that control is no longer one this observation holds', $line);
        return undef;
    }

    # Every point this action names must be inside the image it named.
    for my $point ($line =~ /"x":(-?\d+)/g) {
        if ($point < 0 || $point >= $SENT_W) {
            refuse($id, 'point_outside_observation',
                "that point is not inside a ${SENT_W}x${SENT_H} image", $line);
            return undef;
        }
    }

    return $named;
}

# The receipt an accessibility action earns: the `ax` input method, and the effect
# its own read-back proved (never inferred from an after-image, which it has none
# of). A `semantic` check re-reads the control afterwards and answers
# `element_after`, which is the one evidence an accessibility action can bring.
sub ax_response {
    my ($id, $extra, $effect, $line) = @_;
    my $head = envelope($id, $BOOT, 1);
    my ($kind) = $line =~ /"check":"([^"]*)"/;
    $kind = 'none' unless defined $kind;
    my ($named) = $line =~ /"observation_id":"([^"]*)"/;
    my $before = defined $named ? qq(,"observation_id_before":"$named") : '';

    if ($kind eq 'semantic') {
        my $after = qq("element_after":{"present":true,"role":"AXButton",)
            . qq("label":"Save","enabled":true});
        $extra = $extra eq '' ? $after : qq($extra,$after);
        $effect = 'not_observed' if $effect eq 'unknown';
    }
    $extra = "$extra," if $extra ne '';

    my $check = $kind eq 'semantic'
        ? qq("check":{"kind":"semantic"})
        : qq("check":{"kind":"none"});

    print qq({"type":"response",$head,"ok":true,$extra)
        . qq("receipt":{"dispatch":"sent","effect":"$effect","input_method":"ax",)
        . qq("foreground_changed":false,)
        . qq("timings_ms":{"input":1,"settle":0,"capture":0,"encode":0}$before,$check}}\n);
}

# An action that dispatches input over the pointer, answered with the evidence it
# asked for: an `image` check really mints an observation and answers a picture, so
# the reply a consumer reads is the helper's shape and not a convenient one.
sub checked_response {
    my ($id, $extra, $line) = @_;
    my ($kind) = $line =~ /"check":"([^"]*)"/;

    if (defined $kind && $kind eq 'image') {
        $OBS_COUNTER++;
        my $obs = "fake-$OBS_COUNTER";
        $OBSERVED{$obs} = 1;
        ok_response($id,
            qq($extra,"mime":"image/jpeg","width":$SENT_W,"height":$SENT_H,"data":"AAA",)
            . qq("observation_id":"$obs","observation_kind":"image",)
            . qq("captured_at_monotonic_ns":1,"frame_seq":$OBS_COUNTER),
            $line, 1, $obs);
        return;
    }

    ok_response($id, $extra, $line);
}

sub request {
    my ($id, $action, $line) = @_;

    if ($ADDRESSED{$action}) {
        return unless defined addressed($id, $action, $line);

        # An accessibility action reports what it itself observed; a pointer
        # action addressed by a control still went out over the pointer.
        if ($action eq 'press') {
            # The helper's own shape: no payload of its own, and a `not_observed`
            # effect, because a clean AX return is a dispatch result and not proof
            # that anything happened.
            ax_response($id, '', 'not_observed', $line);
        }
        elsif ($action eq 'set_value') {
            # Set, then read back. Equal earns `verified`, and the value on the
            # reply is what the control holds NOW — the helper's two fields.
            my ($value) = $line =~ /"value":"([^"]*)"/;
            $value = '' unless defined $value;
            ax_response($id, qq("verified":true,"value":"$value"), 'verified', $line);
        }
        else {
            checked_response($id, '"pong":true', $line);
        }
        return;
    }

    if ($action eq 'elements') {
        $OBS_COUNTER++;
        my $obs = "fake-$OBS_COUNTER";
        $OBSERVED{$obs} = 1;
        $ELEMENTS{$obs} = { e1 => 1 };
        ok_response($id,
            qq("elements":[{"element_ref":"e1","role":"AXButton","label":"Save",)
            . qq("enabled":true,"actions":["press"],"settable":false,)
            . qq("bounds":{"x":10,"y":20,"w":60,"h":24},"path":["Document"],)
            . qq("x":40,"y":32}],)
            . qq("observation_id":"$obs","observation_kind":"semantic",)
            . qq("captured_at_monotonic_ns":1),
            $line);
        return;
    }

    if ($action eq 'screenshot') {
        $OBS_COUNTER++;
        my $obs = "fake-$OBS_COUNTER";
        $OBSERVED{$obs} = 1;
        ok_response($id,
            qq("mime":"image/png","width":$SENT_W,"height":$SENT_H,"data":"AAA",)
            . qq("observation_id":"$obs","observation_kind":"image",)
            . qq("captured_at_monotonic_ns":1,"frame_seq":$OBS_COUNTER),
            $line, 1);
        return;
    }

    if    ($action eq 'boom')  { exit 7; }
    elsif ($action eq 'hang')  { sleep 10; }
    elsif ($action eq 'defer') {
        $deferred = $id;
        print qq({"type":"session_event","sidecar_generation":"$BOOT","session_generation":1,)
            . qq("event_seq":1,"kind":"request_deferred"}\n);
    }
    elsif ($action eq 'late') {
        select(undef, undef, undef, 0.5);
        ok_response($id, '"late":true', $line);
    }
    elsif ($action eq 'flush_exit') {
        ok_response($id, '"pong":true', $line);
        exit 75;
    }
    elsif ($action eq 'stash')       { push @stashed, $id; }
    elsif ($action eq 'flush_stash') {
        ok_response($_, '"stale":true') for @stashed;
        @stashed = ();
        ok_response($id, '"flushed":true', $line);
    }
    elsif ($action eq 'capture')    { ok_response($id, '"data":"AAA"', $line, 1); }
    elsif ($action eq 'no_receipt') { ok_response($id, '"pong":true'); }
    elsif ($action eq 'echo') {
        my ($seq)  = $line =~ /"mutation_seq":(\d+)/;
        my ($ms)   = $line =~ /"deadline_ms":(\d+)/;
        my ($auth) = $line =~ /"authorization_generation":(\d+)/;
        my ($boot) = $line =~ /"sidecar_generation":"([^"]*)"/;
        $seq  = 'null' unless defined $seq;
        $ms   = 'null' unless defined $ms;
        $auth = 'null' unless defined $auth;
        $boot = defined $boot ? qq("$boot") : 'null';
        ok_response($id, qq("seen_mutation_seq":$seq,"seen_deadline_ms":$ms,)
            . qq("seen_authorization_generation":$auth,"seen_sidecar_generation":$boot), $line);
    }
    elsif ($action eq 'hello') {
        # Through the same envelope as every other response, so it carries the
        # session generation too.
        my $head = envelope($id, $BOOT, 1);
        print qq({"type":"response",$head,"ok":true,"protocol_version":$PROTOCOL,)
            . qq("compux_version":"0.0.0-test","actions":["screenshot"],)
            . qq("capabilities":{"input_methods":["foreground_hid"],)
            . qq("controls":["pause","resume","release"],)
            . qq("observations":{"max":3,"ttl_ms":30000}}}\n);
    }
    elsif ($action eq 'dribble') {
        for (1 .. 10) {
            print 'x' x 64;
            select(undef, undef, undef, 0.25);
        }
        sleep 10;
    }
    elsif ($action eq 'oversize') {
        # Builds its own frame to an EXACT byte count, so no receipt is appended:
        # this one is a framing fixture, not a modelled action.
        my ($bytes) = $line =~ /"bytes":(\d+)/;
        $bytes = 512 unless defined $bytes;
        my $head = qq({"type":"response",) . envelope($id, $BOOT, 1) . qq(,"ok":true,"pad":");
        my $tail = qq("}\n);
        my $pad = $bytes - length($head) - length($tail);
        $pad = 1 if $pad < 1;
        print $head . ('x' x $pad) . $tail;
    }
    elsif ($action eq 'unknown_id') { ok_response('r-nobody-sent-this', '"pong":true'); }
    elsif ($action eq 'future_id')  { ok_response('r9999', '"pong":true'); }
    elsif ($action eq 'malformed')  { print qq({not json\n); }
    elsif ($action eq 'history_event') {
        print qq({"type":"event","v":1,"ts":1,"seq":1,"boot_id":"history-a","kind":"observer.gap"}\n);
    }
    elsif ($action eq 'history_ack') {
        print qq({"type":"ack","action":"observe_start","ok":true,"protocol_version":$PROTOCOL}\n);
    }
    elsif ($action eq 'stale_boot') {
        my $head = envelope($id, 'boot-somebody-else', 1);
        print qq({"type":"response",$head,"ok":true,"pong":true}\n);
    }
    elsif ($action eq 'stale_session') {
        my $head = envelope($id, $BOOT, 99);
        print qq({"type":"response",$head,"ok":true,"pong":true}\n);
    }
    elsif ($action eq 'refuse') {
        my $head = envelope($id, $BOOT, 1);
        print qq({"type":"response",$head,"ok":false,"error":"paused","detail":"a pause is installed",)
            . qq("receipt":{"dispatch":"not_sent","effect":"unknown","input_method":"foreground_hid"}}\n);
    }
    else {
        my ($seq) = $line =~ /"mutation_seq":(\d+)/;
        $seq = 'null' unless defined $seq;
        ok_response($id, qq("pong":true,"seen_mutation_seq":$seq), $line);
    }
}
