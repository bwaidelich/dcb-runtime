.PHONY: release do-release

CURRENT := $(shell sed -n 's/^version = "\(.*\)"$$/\1/p' hosts/rust/Cargo.toml | head -n 1)
NEXT := $(shell echo "$(CURRENT)" | awk -F. '{ print $$1 "." $$2 + 1 ".0" }')

# make release [VERSION=X.Y.Z] – bumps the host's version, commits, tags
# vX.Y.Z and pushes both; the tag triggers .github/workflows/release.yml.
# Without VERSION it asks, suggesting the next minor version.
release:
ifdef VERSION
	@$(MAKE) --no-print-directory do-release VERSION=$(VERSION)
else
	@read -p "Version (current $(CURRENT)) [$(NEXT)]: " v; \
		$(MAKE) --no-print-directory do-release VERSION=$${v:-$(NEXT)}
endif

do-release:
	@echo "$(VERSION)" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$' \
		|| { echo "usage: make release [VERSION=X.Y.Z]"; exit 1; }
	@test "$$(git rev-parse --abbrev-ref HEAD)" = main \
		|| { echo "not on main"; exit 1; }
	@git diff --quiet HEAD \
		|| { echo "working tree not clean"; exit 1; }
	@! git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null \
		|| { echo "tag v$(VERSION) exists"; exit 1; }
	sed -i.bak '1,/^version = /s/^version = ".*"$$/version = "$(VERSION)"/' hosts/rust/Cargo.toml
	rm hosts/rust/Cargo.toml.bak
	cd hosts/rust && cargo update -p dcb-runtime-host
	git commit -am "Release $(VERSION)"
	git tag v$(VERSION)
	git push origin main v$(VERSION)
