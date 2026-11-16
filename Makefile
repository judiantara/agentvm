MAKEFLAGS += --always-make --no-print-directory --silent

# Default action builds both the VM assets and the CLI binary
all: vm cli

# Build the microVM guest image and kernel derivations via Flake
vm:
	@echo "🔨 Building AgentVM guest image and kernel..."
	nix build .#microvmImage

# Build the Rust CLI binary locally in release mode
cli:
	@echo "🦀 Compiling agentvm binary..."
	cargo build --release

run: vm
	@echo "🚀 Launching agentvm..."
	AGENTVM_RUNNER=$$(pwd)/result/bin/microvm-run cargo run --release -- $(ARGS)

# Install the wrapped agentvm (VM runner and virtiofsd baked in) into the user profile
install: vm
	@echo "📦 Installing agentvm into your Nix profile..."
	nix profile remove agentvm
	nix profile add .

# Clean up build artifacts and temporary files
clean:
	@echo "🧹 Cleaning build artifacts and local symlinks..."
	cargo clean
	rm -f result-vm result-vm-*
