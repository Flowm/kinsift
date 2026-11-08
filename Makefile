# Convenience targets. `make help` lists them.

IMAGE    ?= kinsift
TAG      ?= dev
DIST     ?= dist
# Platform for the Docker-built binary; defaults to this machine's architecture.
PLATFORM ?= linux/$(shell uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')

.DEFAULT_GOAL := help
.PHONY: help build build-docker image image-multiarch test lint fmt clean

help: ## List targets
	@grep -hE '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-16s %s\n", $$1, $$2}'

build: ## Native release binary with the host toolchain -> target/release/kinsift
	cargo build --release --locked -p kinsift

build-docker: ## Static Linux binary built in Docker -> dist/kinsift (PLATFORM=linux/arm64 to cross-build)
	docker buildx build --platform $(PLATFORM) --target binary --output type=local,dest=$(DIST) .
	@echo "built $(DIST)/kinsift for $(PLATFORM)"

image: ## Docker image for this machine -> $(IMAGE):$(TAG)
	docker buildx build --platform $(PLATFORM) --load -t $(IMAGE):$(TAG) .

image-multiarch: ## Build the image for amd64 and arm64 (no --load; add PUSH=1 to push)
	docker buildx build --platform linux/amd64,linux/arm64 -t $(IMAGE):$(TAG) $(if $(PUSH),--push,) .

test: ## Run all tests
	cargo test --all

lint: ## rustfmt check and clippy
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt --all

clean: ## Remove build output
	cargo clean
	rm -rf $(DIST)
