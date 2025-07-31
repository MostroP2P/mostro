SHELL := $(shell which bash)
VERSION := $(shell grep "^version = " Cargo.toml | sed "s/version = \"\(.*\)\"/\1/")

docker-build:
	@set -o pipefail; \
	cd docker && \
	set -a && source .env && set +a && \
	mkdir -p config/lnd && \
	echo "Checking LND files..." && \
	echo "LND_CERT_FILE=$${LND_CERT_FILE}" && \
	echo "LND_MACAROON_FILE=$${LND_MACAROON_FILE}" && \
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
	echo "Building and pushing arkanoider/mostro:$$VERSION to Docker Hub"; \
	docker buildx build -f docker/dockerfile-startos --tag arkanoider/mostro:$$VERSION --platform=linux/amd64,linux/arm64 --push .

docker-build-startos:
	@set -o pipefail; \
	cd docker && \
	docker compose build mostro-startos

