#!/bin/bash
echo '## recalculate'
cat out/recalculate.md || true
echo
grep -E 'Maximum resident set size|Elapsed' out/recalculate-time.txt || true
echo
echo '## edit'
echo '```'
cat out/edit.txt || true
echo '```'
echo
echo '## threads'
echo '```'
cat out/stream.txt || true
grep -H 'recalculated' out/scale-*/*.txt || true
echo '```'
