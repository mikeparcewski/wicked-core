#!/bin/sh
while IFS= read -r line; do
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"WKRTURN:%s"}]}}\n' "$line"
printf '{"type":"result","result":"ok","usage":{"input_tokens":1,"output_tokens":1}}\n' "$line"
done
