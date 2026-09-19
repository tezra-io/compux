#!/usr/bin/env perl
# Test-only fake compux sidecar: line-framed JSON, no native code. It exists so
# `Compux.Transport` can be driven against a REAL Port (fragments, interleaving,
# a process that exits) without the Rust binary, the network, or host state.
#
# Autoflush ($| = 1) so each reply reaches the Port immediately (a block-buffered
# write to a pipe would hang the reader).
#
# It speaks protocol 7 framing: every inbound line is a `request` or a `control`
# frame carrying a `request_id`, and every reply echoes that id.
#
# Request actions:
#   hello         -> the identity handshake (protocol 7, a boot generation)
#   boom          -> exit 7                     (a sidecar death mid-request)
#   hang          -> sleep, never reply         (the action deadline)
#   defer         -> announce a session_event, then hold the response back
#   late          -> reply after half a second  (a response past its deadline)
#   flush_exit    -> reply, THEN exit 75        (the capture-stall fail-fast: an
#                                                IDLE exit, which no reply carries)
#   echo          -> reply naming the envelope fields the request carried
#   dribble       -> 10 unterminated 64-byte chunks 250 ms apart, then sleep
#   oversize      -> one response padded to "bytes" total, split by the line limit
#   unknown_id    -> a well-formed response naming a request nobody sent
#   stale_boot    -> a response from a different sidecar generation
#   stale_session -> a response from a different session generation
#   malformed     -> a line that is not JSON
#   history_event -> an unsolicited computer-history `event` frame
#   receipt       -> ok, with a receipt
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

my $deferred;

while (my $line = <STDIN>) {
    my ($type)   = $line =~ /"type":"([^"]*)"/;
    my ($id)     = $line =~ /"request_id":"([^"]*)"/;
    my ($action) = $line =~ /"action":"([^"]*)"/;
    $type   = ''  unless defined $type;
    $id     = ''  unless defined $id;
    $action = ''  unless defined $action;

    if ($type eq 'control') { control($id, $action); }
    else                    { request($id, $action, $line); }
}

sub envelope {
    my ($id, $boot, $session) = @_;
    return qq("request_id":"$id","sidecar_generation":"$boot","session_generation":$session);
}

sub ok_response {
    my ($id, $extra) = @_;
    my $head = envelope($id, $BOOT, 1);
    print qq({"type":"response",$head,"ok":true,$extra}\n);
}

sub control {
    my ($id, $action) = @_;
    exit 9 if $CONTROL_MODE eq 'exit';
    return if $CONTROL_MODE eq 'silent';

    my $in_flight = defined $deferred ? qq("$deferred") : 'null';
    my $head = envelope($id, $BOOT, 1);
    print qq({"type":"control_ack",$head,"action":"$action","ok":true,)
        . qq("authorization_generation":2,"in_flight_request_id":$in_flight}\n);

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
        ok_response($id, '"late":true');
    }
    elsif ($action eq 'flush_exit') {
        ok_response($id, '"pong":true');
        exit 75;
    }
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
            . qq("seen_authorization_generation":$auth,"seen_sidecar_generation":$boot));
    }
    elsif ($action eq 'hello') {
        print qq({"type":"response","request_id":"$id","ok":true,"protocol_version":$PROTOCOL,)
            . qq("sidecar_generation":"$BOOT","compux_version":"0.0.0-test",)
            . qq("actions":["screenshot"],"capabilities":{"input_methods":["foreground_hid"],)
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
        my ($bytes) = $line =~ /"bytes":(\d+)/;
        $bytes = 512 unless defined $bytes;
        my $head = qq({"type":"response",) . envelope($id, $BOOT, 1) . qq(,"ok":true,"pad":");
        my $tail = qq("}\n);
        my $pad = $bytes - length($head) - length($tail);
        $pad = 1 if $pad < 1;
        print $head . ('x' x $pad) . $tail;
    }
    elsif ($action eq 'unknown_id')    { ok_response('r-nobody-sent-this', '"pong":true'); }
    elsif ($action eq 'malformed')     { print qq({not json\n); }
    elsif ($action eq 'history_event') {
        print qq({"type":"event","v":1,"ts":1,"seq":1,"boot_id":"history-a","kind":"observer.gap"}\n);
    }
    elsif ($action eq 'stale_boot') {
        my $head = envelope($id, 'boot-somebody-else', 1);
        print qq({"type":"response",$head,"ok":true,"pong":true}\n);
    }
    elsif ($action eq 'stale_session') {
        my $head = envelope($id, $BOOT, 99);
        print qq({"type":"response",$head,"ok":true,"pong":true}\n);
    }
    elsif ($action eq 'receipt') {
        ok_response($id, qq("receipt":{"dispatch":"sent","effect":"not_observed",)
            . qq("input_method":"foreground_hid","timings_ms":{"input":12,"settle":80,"capture":140}}));
    }
    elsif ($action eq 'refuse') {
        my $head = envelope($id, $BOOT, 1);
        print qq({"type":"response",$head,"ok":false,"error":"paused","detail":"a pause is installed",)
            . qq("receipt":{"dispatch":"not_sent","effect":"unknown","input_method":"foreground_hid"}}\n);
    }
    else {
        my ($seq) = $line =~ /"mutation_seq":(\d+)/;
        $seq = 'null' unless defined $seq;
        ok_response($id, qq("pong":true,"seen_mutation_seq":$seq));
    }
}
