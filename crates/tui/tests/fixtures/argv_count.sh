#!/bin/sh
# Test fixture only: prints how many argv elements it received, so a test
# can confirm multiple args reach the child as separate argv elements
# (not, say, joined into one string or dropped).
echo "argc: $#"
