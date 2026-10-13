# BROKER=1 (operator decision 2026-10-13): the seat holds only a PLACEHOLDER credential of the right shape and a
# far-future expiry; agentgateway (linux/broker/config.yaml) injects the real one from the broker volume.
if [ "${BROKER:-0}" = 1 ]; then
  case $CLI in
    claude)
      ENVS=$(printf '%s' "$ENVS" | sed 's/ANTHROPIC_API_KEY=[^ ]*//')
      printf '%s' '{"claudeAiOauth":{"accessToken":"sk-ant-oat01-PLACEHOLDER-not-a-token","refreshToken":"sk-ant-ort01-PLACEHOLDER-not-a-token","expiresAt":4102444800000,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}' > "$B/home/claude/.credentials.json" ;;
    pi) sed -i '' 's/stub-key-not-a-secret/PLACEHOLDER-pi-key/' "$B/home/pi/models.json" ;;
    opencode) sed -i '' 's/stub-key-not-a-secret/PLACEHOLDER-opencode-key/' "$B/home/opencode.json" ;;
    codex)
      ENVS=$(printf '%s' "$ENVS" | sed 's/STUB_KEY=[^ ]*//')
      sed -i '' -e '/^env_key = /d' -e 's/^name = "stub"$/name = "stub"\
requires_openai_auth = true/' "$B/home/codex/config.toml"
      J=$(python3 -c 'import base64,json
b=lambda o: base64.urlsafe_b64encode(json.dumps(o).encode()).decode().rstrip("=")
claims={"email":"seat@example.invalid","exp":4102444800,"https://api.openai.com/auth":{"chatgpt_plan_type":"pro","chatgpt_account_id":"acct-PLACEHOLDER"}}
print(b({"alg":"none","typ":"JWT"})+"."+b(claims)+".PLACEHOLDER")')
      printf '%s' "{\"auth_mode\":\"chatgpt\",\"OPENAI_API_KEY\":null,\"tokens\":{\"id_token\":\"$J\",\"access_token\":\"$J\",\"refresh_token\":\"PLACEHOLDER-refresh\",\"account_id\":\"acct-PLACEHOLDER\"},\"last_refresh\":\"2099-01-01T00:00:00Z\"}" > "$B/home/codex/auth.json" ;;
  esac
fi
[ -n "${CMD_OVERRIDE:-}" ] && CMD=$CMD_OVERRIDE
chmod -R a+rwX "$B"
PROXY=http://scf-proxy:8888; PPORT=8888; NOP=$STUB,localhost,127.0.0.1
if [ "${GW:-0}" = 1 ]; then PROXY=http://scf-agw:3000; PPORT=3000; NOP=scf-agw,localhost,127.0.0.1; PORT=${GWPORT:-4000}; fi
