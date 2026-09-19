#!/usr/bin/env perl
# Test-only fake compux sidecar: line-framed JSON, no native code. It exists so
# `Compux.Transport` can be driven against a REAL Port (fragments, interleaving,
# a process that exits) without the Rust binary, the network, or host state.
#
# It is FAITHFUL on the points a library regression could otherwise hide, each
# mirroring the real sidecar (`native/compux/src/wire.rs`, `control.rs`, `gate.rs`):
#
#   * a mutating success carries a receipt, derived the way `Receipt::derive` does
#     it — `dispatch: "sent"`, and `effect: "not_observed"` only when the payload
#     really has `data`, else `unknown`. Whether a request mutates is read off the
#     line's own `mutation_seq`, which is exactly the sidecar's rule;
#   * a TYPELESS line is the protocol-6 shape and there is no legacy mode, so it
#     is refused. Naming an action it is answered with an `ack` carrying THIS
#     protocol version (`wire::untagged` -> `Reply::CaptureAck`); naming none it
#     gets silence;
#   * `hello` answers through the same envelope as every other response, so it
#     carries `session_generation` as well as `sidecar_generation`;
#   * `authorization_generation` COUNTS, one per control, from the gate's initial 1.
#
# Autoflush ($| = 1) so each reply reaches the Port immediately (a block-buffered
# write to a pipe would hang the reader).
#
# Request actions:
#   hello         -> the identity handshake (protocol 7, a boot generation)
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

my $PROTOCOL     = defined $ENV{FAKE_PROTOCOL_VERSION}   ? $ENV{FAKE_PROTOCOL_VERSION}   : 7;
my $BOOT         = defined $ENV{FAKE_SIDECAR_GENERATION} ? $ENV{FAKE_SIDECAR_GENERATION} : 'boot-test';
my $CONTROL_MODE = defined $ENV{FAKE_CONTROL_MODE}       ? $ENV{FAKE_CONTROL_MODE}       : 'ack';

# The gate's own initial value; every control bumps it.
my $AUTH = 1;

my $deferred;
my @stashed;

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

# A mutating request earns a receipt, and only a payload that really carries
# `data` has an after-image to assess.
sub receipt_for {
    my ($line, $has_data) = @_;
    return '' unless $line =~ /"mutation_seq":\d+/;
    my $effect = $has_data ? 'not_observed' : 'unknown';
    return qq(,"receipt":{"dispatch":"sent","effect":"$effect",)
        . qq("input_method":"foreground_hid","timings_ms":{"input":1,"settle":0,"capture":0}});
}

sub ok_response {
    my ($id, $extra, $line, $has_data) = @_;
    my $head = envelope($id, $BOOT, 1);
    my $receipt = defined $line ? receipt_for($line, $has_data) : '';
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
        . qq("error":"protocol 7 needs a tagged frame; this line has no type"}\n);
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

sub request {
    my ($id, $action, $line) = @_;

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
            . qq("controls":["pause","resume","release"]}}\n);
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
