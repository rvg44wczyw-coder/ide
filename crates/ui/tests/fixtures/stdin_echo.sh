#!/bin/sh
# Test fixture only: ignores argv (including the "-p" flag run_command
# always passes), copies stdin to stdout verbatim so tests can observe
# exactly what run_command wrote to the child's stdin.
cat
