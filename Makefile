help:
	@grep '^[^#[:space:]].*:' Makefile

license-check:
	docker run -it --rm -v $(shell pwd):/github/workspace apache/skywalking-eyes header check

license-fix:
	docker run -it --rm -v $(shell pwd):/github/workspace apache/skywalking-eyes header fix

