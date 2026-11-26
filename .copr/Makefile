# COPR "make srpm" entry point (https://docs.pagure.org/copr.copr/user_documentation.html#make-srpm).
# COPR runs this with network access in a Fedora chroot, checked out at the
# commit being built: vendor the crates and write the source RPM to $(outdir).
outdir ?= $(CURDIR)/dist/srpm

srpm:
	dnf -y install cargo git rpm-build
	git config --global --add safe.directory '*'
	packaging/vendor-tarball.sh
	rpmbuild -bs --define "_sourcedir $(CURDIR)/dist/src" \
	    --define "_srcrpmdir $(outdir)" packaging/rpm/blessh.spec
