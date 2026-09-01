#!/bin/sh
# Test fixture only: echoes its first argv element verbatim, so a test can
# confirm CargoPanel passes the cargo subcommand as a single explicit argv
# element rather than through a shell (which would interpret metacharacters
# instead of passing them through byte-for-byte).
echo "argv: $1"
