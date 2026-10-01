SHELL := $(shell which bash)
VERSION := $(shell grep "^version = " Cargo.toml | sed "s/version = \"\(.*\)\"/\1/")

# Notes on the docker/config handling below, since none of it is obvious:
#
# - `install -d -m 700` applies the mode to an existing directory too, so it is
#   guarded with `[ -d config ]` everywhere. Only the run that first creates
#   the directory gets to decide its mode; an operator who deliberately opened
#   `config` up to a group keeps it across every later `make docker-build` or
#   `make docker-up`. `config/lnd` is unguarded because docker-build owns it:
#   it holds nothing but the credentials this target installs.
# - `install -m 600` onto an existing 0644 macaroon is safe as it stands: it
#   unlinks the destination and creates it with the owner-only bits already
#   applied, rather than truncating in place and chmod'ing at the end. Measured
#   by polling the mode throughout a 300 MB copy — only 0600 is ever observed.
# - The mostro container has to run as whoever owns `config`: the macaroon
#   there is 0600 and the daemon also writes mostro.db beside it. Deriving the
#   uid:gid from the directory covers both the default (docker-build created it
#   as you) and the documented `chown -R 1000:1000` handover. uid 0 is refused
#   rather than used: a root-owned `config` (a `sudo make docker-build`, say)
#   would otherwise drop the unprivileged user the image runs as.
# - Which is also why docker-build derives `-o/-g` from `config` when it runs
#   as root over a directory that is not. `install` unlinks the destination and
#   recreates it as the invoking user, so `sudo make docker-build` over a
#   `config` handed to 1000 would leave a `root:root 0600` macaroon that
#   docker-up then starts a uid 1000 container against — mostrod failing at the
#   LND connection with nothing pointing at ownership. A `config` that is
#   itself root-owned is left alone: docker-up refuses that case outright.
# - That refusal matches both spellings compose takes for uid 0, the numeric 0
#   and the name `root`, since MOSTRO_CONTAINER_USER is passed through to the
#   `user:` key verbatim. Any other name is resolved inside the image, where
#   mostrouser is the only account besides root.

docker-build:
	@set -o pipefail; \
	cd docker && \
	{ [ -d config ] || install -d -m 700 config; } && \
	config_owner="$$(stat -c '%u:%g' config 2>/dev/null || stat -f '%u:%g' config)" && \
	install_owner="" && \
	if [ "$$(id -u)" = 0 ] && [ "$${config_owner%%:*}" != 0 ]; then \
		install_owner="-o $${config_owner%%:*} -g $${config_owner##*:}"; \
		echo "Running as root: installing config/lnd as $${config_owner}, the owner of config"; \
	fi && \
	install -d -m 700 $${install_owner} config/lnd && \
	echo "Checking LND files..." && \
	echo "LND_CERT_FILE=$${LND_CERT_FILE}" && \
	echo "LND_MACAROON_FILE=$${LND_MACAROON_FILE}" && \
	if [ -z "$${LND_CERT_FILE}" ]; then \
		echo "Error: LND_CERT_FILE environment variable is not set"; \
		echo "Usage: LND_CERT_FILE=/path/to/tls.cert LND_MACAROON_FILE=/path/to/admin.macaroon make docker-build"; \
		exit 1; \
	fi && \
	if [ -z "$${LND_MACAROON_FILE}" ]; then \
		echo "Error: LND_MACAROON_FILE environment variable is not set"; \
		echo "Usage: LND_CERT_FILE=/path/to/tls.cert LND_MACAROON_FILE=/path/to/admin.macaroon make docker-build"; \
		exit 1; \
	fi && \
	if [ ! -f "$${LND_CERT_FILE}" ]; then \
		echo "Error: LND cert file not found at: $${LND_CERT_FILE}"; \
		exit 1; \
	fi && \
	if [ ! -f "$${LND_MACAROON_FILE}" ]; then \
		echo "Error: LND macaroon file not found at: $${LND_MACAROON_FILE}"; \
		exit 1; \
	fi && \
	echo "Copying LND cert and macaroon to docker config" && \
	install -m 644 $${install_owner} "$${LND_CERT_FILE}" config/lnd/tls.cert && \
	install -m 600 $${install_owner} "$${LND_MACAROON_FILE}" config/lnd/admin.macaroon && \
	echo "Wrote config/lnd/tls.cert (mode 644) and config/lnd/admin.macaroon (mode 600)" && \
	echo "config/lnd is mode 700, and config keeps the mode it was created with (700 unless you" && \
	echo "changed it): settings.toml holds nsec_privkey and mostro.db lands there too" && \
	echo "The mostro container runs as the owner of docker/config, which make docker-up derives" && \
	echo "and prints. Set MOSTRO_CONTAINER_USER=uid:gid to override it. A root-owned config" && \
	echo "directory is refused there; under sudo this target installs the credentials as the" && \
	echo "owner of config, so a directory already handed over stays readable by the container." && \
	echo "Building docker image" && \
	docker compose build

docker-up:
	@set -o pipefail; \
	cd docker && \
	echo "Copying Nostr relay config" && \
	{ [ -d config ] || install -d -m 700 config; } && \
	mkdir -p config/relay && \
	cp -v ./relay_config.toml config/relay/config.toml && \
	export MOSTRO_CONTAINER_USER="$${MOSTRO_CONTAINER_USER:-$$(stat -c '%u:%g' config 2>/dev/null || stat -f '%u:%g' config)}" && \
	case "$${MOSTRO_CONTAINER_USER%%:*}" in \
	0|root) \
		echo "Error: refusing to run the mostro container as root." >&2; \
		echo "MOSTRO_CONTAINER_USER is $${MOSTRO_CONTAINER_USER}. It defaults to the owner of" >&2; \
		echo "docker/config, which a run under sudo leaves as root." >&2; \
		echo "Hand that directory to an unprivileged account (chown -R 1000:1000 config)," >&2; \
		echo "or export MOSTRO_CONTAINER_USER=uid:gid with a non-zero uid that can read it." >&2; \
		exit 1;; \
	esac && \
	echo "Running mostro as $${MOSTRO_CONTAINER_USER} (MOSTRO_CONTAINER_USER; defaults to the owner of docker/config)" && \
	echo "Starting services" && \
	docker compose up -d

docker-relay-up:
	@set -o pipefail; \
	cd docker && \
	echo "Copying Nostr relay config" && \
	{ [ -d config ] || install -d -m 700 config; } && \
	mkdir -p config/relay && \
	cp -v ./relay_config.toml config/relay/config.toml && \
	echo "Starting Nostr relay" && \
	docker compose up -d nostr-relay

docker-down:
	@set -o pipefail; \
	cd docker && \
	docker compose down

docker-startos:
	@set -o pipefail; \
	VERSION=$$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/'); \
	echo "Building and pushing mostrop2p/mostro-startos:$$VERSION to Docker Hub"; \
	docker buildx build -f docker/dockerfile-startos --tag mostrop2p/mostro-startos:$$VERSION --platform=linux/amd64,linux/arm64 --push .

docker-build-startos:
	@set -o pipefail; \
	cd docker && \
	docker compose build mostro-startos

