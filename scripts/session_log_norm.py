"""Normalizes a session-sim log for compare_session_logs.sh --ids (P2-3 S3b): masks wall-clock
timings, entry ids and digests (16 hex digits), transfer ids (15-20 decimal digits) and byte
counts, sorts the elements of every [...] list and every ", "-separated per-version listing
within a line, and sorts the lines. Two logs equal after this record the same events, labelled
and ordered by other ids. Reads stdin, writes stdout."""
import re
import sys

sys.stdout.reconfigure(newline='\n')


def norm(line):
    line = re.sub(r'[0-9]+(\.[0-9]+)? ?(ms|us|µs|s)\b', 'T', line)
    line = re.sub(r'\b[0-9a-f]{16}\b', 'ID', line)
    line = re.sub(r'T?[0-9]{15,20}\b', 'ID', line)
    line = re.sub(r'[0-9]+ B\b', 'NB', line)
    line = re.sub(r'[0-9]+ B/s', 'NB/s', line)
    # Sort the elements of innermost [...] lists (quoted strings with braces are kept whole).
    line = re.sub(r'\[([^\[\]]*)\]', lambda m: '[' + ', '.join(sorted((e.strip() for e in re.findall(r'\s*("[^"]*"|[^,]+)', m.group(1))))) + ']', line)
    # "label: a, b" listings of several versions (e.g. "9's touches at m1: x, y").
    m = re.match(r'^(.*?: )(m\d+s\d+@.*)$', line)
    if m:
        line = m.group(1) + ', '.join(sorted(s.strip() for s in re.split(r', (?=m\d+s\d+@)', m.group(2))))
    return line


print('\n'.join(sorted(norm(l.rstrip('\n')) for l in sys.stdin)))
