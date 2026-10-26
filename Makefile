RUNTIME := runtime
EXT := extension
BIN := task-x-runtime
CODE ?= code
VERSION = $(shell node -p "require('./$(EXT)/package.json').version")
VSIX = $(EXT)/task-x-$(VERSION).vsix
# GitHub repository for releases, and the stable asset name `make add` downloads.
REPO ?= its-ash/task-x
ASSET := task-x.vsix
# VS Code's own `code` CLI prints Node's DEP0169 warning (its `url.parse()` call, not our code).
# Its wrapper moves NODE_OPTIONS to VSCODE_NODE_OPTIONS, which cli.js never reads, so the flag
# cannot disable the warning. Each install step captures the CLI's output, drops exactly those two
# lines, and passes the CLI's own exit code through (a pipe would report grep's status instead).
CODE_FILTER = grep -vE '^\(node:[0-9]+\) \[DEP0169\]|^\(Use `Code --trace-deprecation'

.PHONY: run build deploy install add

run:
	cargo build --manifest-path $(RUNTIME)/Cargo.toml
	npm --prefix $(EXT) install --no-audit --no-fund
	npm --prefix $(EXT) run build
	code --extensionDevelopmentPath="$(CURDIR)/$(EXT)"

build:
	cargo build --release --manifest-path $(RUNTIME)/Cargo.toml
	mkdir -p $(EXT)/bin
	@# Replace, not overwrite: macOS kills a signed binary that is rewritten in place.
	rm -f $(EXT)/bin/$(BIN)
	cp $(RUNTIME)/target/release/$(BIN) $(EXT)/bin/$(BIN)
	npm --prefix $(EXT) ci --no-audit --no-fund
	npm --prefix $(EXT) run build

# Builds, packages, and installs the extension into VS Code (replacing an installed copy).
# Use CODE=code-insiders for VS Code Insiders. The old "anvil-agent" build is removed first.
install: build
	rm -f $(EXT)/*.vsix
	npm --prefix $(EXT) run package
	@$(CODE) --uninstall-extension its-ash.anvil-agent >/dev/null 2>&1 || true
	@mk_cdp=$$(mktemp); $(CODE) --install-extension $(VSIX) --force > "$$mk_cdp" 2>&1; status=$$?; $(CODE_FILTER) "$$mk_cdp" >&2 || true; rm -f "$$mk_cdp"; [ $$status -eq 0 ] || exit $$status
	@echo "Installed $(VSIX). Reload VS Code windows to use it."

# Builds the .vsix, pushes main, and publishes the .vsix on the GitHub release v<version>
# (package.json "version"). A release that exists already gets the new files.
deploy: build
	rm -f $(EXT)/*.vsix
	npm --prefix $(EXT) run package
	cp $(VSIX) $(EXT)/$(ASSET)
	git checkout main
	git add -A
	git diff --cached --quiet || git commit -m "$$(copilot -sp 'Analyze the staged git changes and generate a concise commit message. Output ONLY the commit message. Do not execute any commands. Do not include quotes, markdown, explanation, or bullet points.')"
	git push origin main
	@if gh release view v$(VERSION) --repo $(REPO) >/dev/null 2>&1; then \
		gh release upload v$(VERSION) $(VSIX) $(EXT)/$(ASSET) --repo $(REPO) --clobber; \
	else \
		gh release create v$(VERSION) $(VSIX) $(EXT)/$(ASSET) --repo $(REPO) --target main --title "task-x $(VERSION)" \
			--notes "Download $(ASSET), then run: code --install-extension $(ASSET)"; \
	fi
	@echo "Published https://github.com/$(REPO)/releases/tag/v$(VERSION)"

# Downloads the .vsix from the latest GitHub release and installs it into VS Code.
# Uses gh when it is installed (works for private repositories), else curl.
add:
	@set -e; tmp="$$(mktemp -d)"; trap 'rm -rf "$$tmp"' EXIT; \
	echo "Downloading $(ASSET) from the latest release of $(REPO)"; \
	if command -v gh >/dev/null 2>&1; then \
		gh release download --repo $(REPO) --pattern $(ASSET) --dir "$$tmp"; \
	else \
		curl -fSL --progress-bar -o "$$tmp/$(ASSET)" "https://github.com/$(REPO)/releases/latest/download/$(ASSET)"; \
	fi; \
	$(CODE) --uninstall-extension its-ash.anvil-agent >/dev/null 2>&1 || true; \
	mk_cdp=$$(mktemp); { $(CODE) --install-extension "$$tmp/$(ASSET)" --force > "$$mk_cdp" 2>&1; }; status=$$?; \
	$(CODE_FILTER) "$$mk_cdp" >&2 || true; rm -f "$$mk_cdp" || true; \
	[ $$status -eq 0 ] || exit $$status; \
	echo "Installed task-x. Reload VS Code windows to use it."
