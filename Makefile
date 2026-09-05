SHELL := $(shell which bash)
VERSION := $(shell grep "^version = " Cargo.toml | sed "s/version = \"\(.*\)\"/\1/")

docker-build:
	@set -o pipefail; \
	cd docker && \
	mkdir -p config/lnd && \
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
	cp -v $${LND_CERT_FILE} config/lnd/tls.cert && \
	cp -v $${LND_MACAROON_FILE} config/lnd/admin.macaroon && \
	echo "Building docker image" && \
	docker compose build

docker-up:
	@set -o pipefail; \
	cd docker && \
	echo "Copying Nostr relay config" && \
	mkdir -p config/relay && \
	cp -v ./relay_config.toml config/relay/config.toml && \
	echo "Starting services" && \
	docker compose up -d

docker-relay-up:
	@set -o pipefail; \
	cd docker && \
	echo "Copying Nostr relay config" && \
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

# cargo-mutants tests one mutant at a time by default (verified against
# 27.1.0: a single scratch dir on a 16-CPU host with no -j). This target
# sets it explicitly anyway, because the suite genuinely cannot tolerate
# more: the LNURL tests bind a fixed host port, so parallel workers collide
# on the listener, and a test that fails for its own reasons scores as a
# killed mutant — silently inflating the number this target exists to
# measure.
#
# MOSTRO_TEST_LN_PORT moves those tests off 8080 in case something on the
# host already holds it. It does NOT make the suite hermetic: two workers
# would still collide with each other on 18080. Serialising is what makes
# the run trustworthy; the port override only dodges a pre-existing
# listener.
#
# The 18080 here deliberately differs from the code's own 8080 default, so
# `cargo test` and `make mutation-test` do bind different ports. That is the
# point: plain `cargo test` keeps exercising the default path, and only this
# target — which runs the suite hundreds of times over — steps aside from a
# port a developer machine is likely to have in use.
#
# ARGS is spliced into the shell command as plain text — only pass
# hand-typed, trusted values (e.g. `make mutation-test ARGS="--file
# src/foo.rs"`). Never build ARGS from PR-diff filenames or other
# attacker-controlled input; that class of data must be turned into a bash
# array and passed to `cargo mutants` directly (see the PR job in
# .github/workflows/mutation.yml).
mutation-test:
	@set -o pipefail; \
	CARGO_MUTANTS_JOBS=1 MOSTRO_TEST_LN_PORT=$${MOSTRO_TEST_LN_PORT:-18080} cargo mutants $(ARGS)
