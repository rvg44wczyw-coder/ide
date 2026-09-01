#!/bin/sh
# Test fixture only: ignores argv, prints one line to stdout, one to
# stderr, then another to stdout -- so tests can confirm both streams are
# captured and interleaved into CargoPanel::output.
echo "line1"
echo "err1" 1>&2
echo "line2"
