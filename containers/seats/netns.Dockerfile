# wicked seat network sidecar. One per seat: it owns the seat's network namespace (the seat joins
# with `--network container:<this>` and has no capabilities, so it cannot change the rules) and
# allows output ONLY to loopback, established flows and the gateway endpoints given as
# `<ip>:<port>` arguments. An `--internal` network alone still reaches the VM host's services
# through the bridge address; this closes that. OSS base (alpine) + iptables.
ARG BASE=alpine:3.22
FROM alpine:3.22
RUN apk add --no-cache iptables ip6tables
RUN cat > /usr/local/bin/wicked-netns <<'EOF'
#!/bin/sh
set -eu
iptables -P OUTPUT DROP
iptables -A OUTPUT -o lo -j ACCEPT
iptables -A OUTPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
for ep in "$@"; do
  ip=${ep%:*}; port=${ep##*:}
  iptables -A OUTPUT -p tcp -d "$ip" --dport "$port" -j ACCEPT
done
ip6tables -P OUTPUT DROP
ip6tables -A OUTPUT -o lo -j ACCEPT
echo wicked-netns-ready
exec sleep 2147483647
EOF
RUN chmod 0755 /usr/local/bin/wicked-netns
LABEL wicked.seat.cli=netns
ENTRYPOINT []
