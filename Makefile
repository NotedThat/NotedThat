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
	@tmp=$$(docker create $(IMAGE)) \
	  && docker cp $$tmp:/usr/local/bin/notedthat-mcp-stdio $(BIN) \
	  && docker rm $$tmp >/dev/null
	@echo "installed: $(BIN)"
