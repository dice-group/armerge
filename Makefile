.PHONY: fmt
fmt:
	curl -fsSL "https://raw.githubusercontent.com/dice-group/tentris-cpp-coding-guidelines/main/.rustfmt.toml" -o ".rustfmt.toml"
	cargo +nightly fmt