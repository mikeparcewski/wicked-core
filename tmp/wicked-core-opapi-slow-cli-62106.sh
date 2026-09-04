#!/bin/sh
while IFS= read -r line; do
sleep 0.5
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"SLOW:%s"}]}}\n' "$line"
printf '{"type":"result","result":"ok","usage":{"input_tokens":1,"output_tokens":1}}\n'
done
