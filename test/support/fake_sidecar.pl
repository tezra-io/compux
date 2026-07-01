#!/usr/bin/env perl
# Test-only fake compux sidecar: a line-framed JSON echo, no native code.
# Exercises the `Compux.PortDriver` transport framing without the Rust binary.
# Autoflush ($| = 1) so each one-line reply reaches the Port immediately
# (a block-buffered shell `printf` to a pipe would hang the reader).
#
#   * a line matching "boom"  -> exit non-zero       (tests :sidecar_exited)
#   * a line matching "hang"  -> sleep               (tests the action timeout)
#   * a line matching "hello" -> identity handshake  (tests Compux.start/1)
#   * otherwise               -> reply {"ok":true,"pong":true}
use strict;
use warnings;
$| = 1;

while (my $line = <STDIN>) {
    if ($line =~ /boom/) {
        exit 7;
    }
    elsif ($line =~ /hang/) {
        sleep 10;
    }
    elsif ($line =~ /hello/) {
        print qq({"ok":true,"protocol_version":1,"compux_version":"0.0.0-test","actions":["screenshot"]}\n);
    }
    else {
        print qq({"ok":true,"pong":true}\n);
    }
}
