RUNTIME := runtime
BIN := task-x-runtime
# The version is the runtime's: `version` in the [package] table of runtime/Cargo.toml.
VERSION = $(shell grep -m1 '^version = ' $(RUNTIME)/Cargo.toml | cut -d'"' -f2)
# GitHub repository for releases.
REPO ?= its-ash/task-x
# The release asset for this machine, e.g. task-x-runtime-darwin-arm64. `make deploy` uploads the
# one it builds; `make add` downloads the one for the machine it runs on.
OS := $(shell uname -s | tr '[:upper:]' '[:lower:]')
ARCH := $(shell uname -m)
ASSET := $(BIN)-$(OS)-$(ARCH)
# Where `make install` and `make add` put the runtime: $(PREFIX)/bin/task-x-runtime.
PREFIX ?= $(HOME)/.local

.PHONY: run build deploy install add bump link-vscode

# Debug build, then an interactive session in this folder.
run:
	cargo run --manifest-path $(RUNTIME)/Cargo.toml -- chat

# Builds the release runtime: $(RUNTIME)/target/release/$(BIN).
build:
	cargo build --release --manifest-path $(RUNTIME)/Cargo.toml

# Builds the runtime and installs it as $(PREFIX)/bin/$(BIN) (default ~/.local/bin), replacing
# an installed copy. Use PREFIX=/usr/local (with sudo) for a system-wide install.
install: build
	mkdir -p $(PREFIX)/bin
	@# Replace, not overwrite: macOS kills a signed binary that is rewritten in place.
	rm -f $(PREFIX)/bin/$(BIN)
	cp $(RUNTIME)/target/release/$(BIN) $(PREFIX)/bin/$(BIN)
	@echo "Installed $(PREFIX)/bin/$(BIN)."
	@case ":$$PATH:" in *":$(PREFIX)/bin:"*) ;; *) echo "$(PREFIX)/bin is not on your PATH; add it to run $(BIN) by name." ;; esac
	@$(MAKE) --no-print-directory link-vscode

# Installed task-x VS Code extensions run the runtime in their own bin/ folder, which no update
# reaches. Each one's copy becomes a link to $(PREFIX)/bin/$(BIN), so every install (and its MCP
# tools) is what VS Code runs after a window reload. Running processes keep their old binary.
link-vscode:
	@for d in $(HOME)/.vscode/extensions/its-ash.task-x-* $(HOME)/.vscode-insiders/extensions/its-ash.task-x-* $(HOME)/.cursor/extensions/its-ash.task-x-*; do \
		[ -d "$$d" ] || continue; \
		mkdir -p "$$d/bin"; \
		rm -f "$$d/bin/$(BIN)"; \
		ln -s "$(PREFIX)/bin/$(BIN)" "$$d/bin/$(BIN)"; \
		echo "Linked $$d/bin/$(BIN) to $(PREFIX)/bin/$(BIN); reload VS Code to use it."; \
	done

# Raises the patch version in runtime/Cargo.toml until no release v<version> exists, so each
# deploy is a new release. A deploy that failed before its release keeps its version.
bump:
	@v="$(VERSION)"; \
	while gh release view "v$$v" --repo $(REPO) >/dev/null 2>&1; do v="$${v%.*}.$$(( $${v##*.} + 1 ))"; done; \
	if [ "$$v" != "$(VERSION)" ]; then \
		perl -pi -e 'if (!$$done && s/^version = "[^"]*"/version = "'"$$v"'"/) { $$done = 1 }' $(RUNTIME)/Cargo.toml; \
		echo "Version $(VERSION) is released; now $$v"; \
	fi

# Raises the version (bump), builds the runtime, pushes main, and publishes the binary for this
# machine on a new GitHub release v<version>. `bump` runs first, so the build sees the new version.
deploy: bump build
	cp $(RUNTIME)/target/release/$(BIN) $(RUNTIME)/target/release/$(ASSET)
	git checkout main
	git add -A
	git diff --cached --quiet || { msg="$$(node scripts/commit-msg.mjs)" && git commit -m "$$msg"; }
	git push origin main
	gh release create v$(VERSION) $(RUNTIME)/target/release/$(ASSET) --repo $(REPO) --target main --title "task-x $(VERSION)" --latest \
		--notes "Download $(ASSET), make it executable (chmod +x), and put it on your PATH as $(BIN). Or, in a clone of the repository: make add"
	@echo "Published https://github.com/$(REPO)/releases/tag/v$(VERSION)"

# Downloads the runtime for this machine from the latest GitHub release and installs it as
# $(PREFIX)/bin/$(BIN). Uses gh when it is installed (works for private repositories), else curl.
add:
	@set -e; tmp="$$(mktemp -d)"; trap 'rm -rf "$$tmp"' EXIT; \
	echo "Downloading $(ASSET) from the latest release of $(REPO)"; \
	if command -v gh >/dev/null 2>&1; then \
		gh release download --repo $(REPO) --pattern $(ASSET) --dir "$$tmp"; \
	else \
		curl -fSL --progress-bar -o "$$tmp/$(ASSET)" "https://github.com/$(REPO)/releases/latest/download/$(ASSET)"; \
	fi; \
	mkdir -p $(PREFIX)/bin; \
	rm -f $(PREFIX)/bin/$(BIN); \
	cp "$$tmp/$(ASSET)" $(PREFIX)/bin/$(BIN); \
	chmod +x $(PREFIX)/bin/$(BIN); \
	echo "Installed $(PREFIX)/bin/$(BIN)."
	@$(MAKE) --no-print-directory link-vscode
