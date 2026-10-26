RUNTIME := runtime
EXT := extension
BIN := task-x-runtime
CODE ?= code
VERSION = $(shell node -p "require('./$(EXT)/package.json').version")
VSIX = $(EXT)/task-x-$(VERSION).vsix

.PHONY: run build deploy install

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
	$(CODE) --install-extension $(VSIX) --force
	@echo "Installed $(VSIX). Reload VS Code windows to use it."

deploy: build
	git checkout main
	git add -A
	git commit -m "$$(copilot -sp 'Analyze the staged git changes and generate a concise commit message. Output ONLY the commit message. Do not execute any commands. Do not include quotes, markdown, explanation, or bullet points.')"
	git push origin main
