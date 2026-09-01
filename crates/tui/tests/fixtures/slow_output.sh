#!/bin/sh
# Test fixture only: ignores argv. Prints a line, sleeps, then prints a
# second line -- lets a test prove CargoPanel delivers output as it
# arrives rather than buffering until the process exits.
echo "before-sleep"
sleep 0.3
echo "after-sleep"
