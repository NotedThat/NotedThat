# NotedThat Makefile — thin wrappers around plain `cargo` and `docker`.
# Daily commands live in DEVELOPMENT.md; this file only automates the few
# multi-step sequences that would otherwise be error-prone to type by hand.

PREFIX ?= $(HOME)/.local
BIN    := $(PREFIX)/bin/notedthat-mcp-stdio
IMAGE  ?= notedthat-server:local

.PHONY: help mcp-stdio mcp-stdio-from-image

help:
	@echo "NotedThat make targets:"
	@echo "  mcp-stdio             Build notedthat-mcp-stdio from source and install to $(BIN)."
	@echo "                        Uses cargo, so it reflects your local sources."
	@echo "  mcp-stdio-from-image  Copy the pre-built binary out of the Docker image ($(IMAGE))"
	@echo "                        into $(BIN). Much faster than cargo, but only reflects what"
	@echo "                        was last built into the image."
	@echo ""
	@echo "Override PREFIX=/some/dir or IMAGE=some:tag to change locations."

mcp-stdio:
	cargo install --path crates/notedthat-mcp-stdio --root $(PREFIX) --force --locked
	@echo "installed: $(BIN)"

mcp-stdio-from-image:
	@mkdir -p $(PREFIX)/bin
	@set -eu; \
	container=''; temp_file=''; \
	cleanup() { \
	  test -z "$$temp_file" || rm -f "$$temp_file"; \
	  test -z "$$container" || docker rm "$$container" >/dev/null 2>&1 || true; \
	}; \
	trap cleanup EXIT HUP INT TERM; \
	container=$$(docker create $(IMAGE)); \
	temp_file=$$(mktemp "$(PREFIX)/bin/.notedthat-mcp-stdio.XXXXXX"); \
	docker cp "$$container:/usr/local/bin/notedthat-mcp-stdio" "$$temp_file"; \
	mv -f "$$temp_file" "$(BIN)"; \
	temp_file=''
	@echo "installed: $(BIN)"
