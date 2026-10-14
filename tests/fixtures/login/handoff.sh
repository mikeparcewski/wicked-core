#!/bin/sh
# review finding 11: login container -> broker handoff under the UID scheme. The login container runs as the
# broker uid 65532 (any uid works for a CLI login), so the 0600 credential it writes is readable by the ingester
# (also 65532), which moves it into the broker volume and writes the PLACEHOLDER into the seat-home volume (as 10001).
. $SPIKE_ENV
LBL="--label wicked.lane=spike-container-floor"; HARD="--read-only --tmpfs /tmp:rw,mode=1777 --cap-drop ALL --security-opt no-new-privileges --network none"
for v in scf-login-tmp scf-broker2 scf-home-fake; do docker volume rm $v >/dev/null 2>&1; docker volume create $LBL $v >/dev/null; done
init() { docker run --rm $LBL --user 0 --read-only --cap-drop ALL --cap-add CHOWN --security-opt no-new-privileges -v $1:/v scf-agent:spike sh -c "chmod 700 /v && chown $2 /v"; }
init scf-login-tmp 65532:65532; init scf-broker2 65532:65532; init scf-home-fake 10001:10001
echo "--- 1. login container (uid 65532, throwaway home)"
docker run --rm $LBL $HARD --user 65532:65532 -e HOME=/home/l -v scf-login-tmp:/home/l -v $L/linux/login:/opt/fake:ro scf-agent:spike sh /opt/fake/fakecli.sh login
echo "--- 2. ingester (uid 65532): move credential into the broker volume"
docker run --rm $LBL $HARD --user 65532:65532 -v scf-login-tmp:/in -v scf-broker2:/broker scf-agent:spike sh -c 'umask 077; mkdir -p /broker/fakecli && cp /in/.fakecli/credentials.json /broker/fakecli/credentials.json && rm -rf /in/.fakecli && ls -ln /broker/fakecli'
docker volume rm scf-login-tmp >/dev/null && echo "throwaway login volume removed"
echo "--- 3. placeholder into the seat home (uid 10001): same shape, dummy token, far-future expiry"
docker run --rm $LBL $HARD --user 10001:10001 -v scf-home-fake:/home/agent scf-agent:spike sh -c 'umask 077; mkdir -p /home/agent/.fakecli && printf "%s\n" "{\"token\":\"PLACEHOLDER\",\"expires_at\":4102444800}" > /home/agent/.fakecli/credentials.json'
echo "--- 4. seat (uid 10001): CLI believes it is signed in; real token absent; broker volume not mounted"
docker run --rm $LBL $HARD --user 10001:10001 -e HOME=/home/agent -v scf-home-fake:/home/agent -v $L/linux/login:/opt/fake:ro scf-agent:spike sh -c 'sh /opt/fake/fakecli.sh status; echo rc=$?; grep -rl "FAKE-TOKEN" /home/agent 2>/dev/null | wc -l | sed "s/^/real-token files in seat: /"'
echo "--- 5. seat uid cannot read the broker volume even if it were mounted by mistake"
docker run --rm $LBL $HARD --user 10001:10001 -v scf-broker2:/broker scf-agent:spike sh -c 'cat /broker/fakecli/credentials.json 2>&1 | head -1'
for v in scf-broker2 scf-home-fake; do docker volume rm $v >/dev/null; done
