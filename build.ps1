docker build --tag "gha-ping" --build-arg git_repo_url="git@github.com:alexobolev/gha-ping.git" .
docker run -it --rm --mount type=bind,source="$(($pwd).path)/local/gha_ping_ed25519",target="/var/gha_ping/key",ro -p 4331:4331/tcp "gha-ping"
