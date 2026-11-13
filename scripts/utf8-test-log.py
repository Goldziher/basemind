"""Keep Cargo output readable when tests emit bytes outside UTF-8."""

import sys

for line in sys.stdin.buffer:
    sys.stdout.buffer.write(line.decode("utf-8", errors="backslashreplace").encode("utf-8"))
