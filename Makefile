example-image:
	docker build -t io-example ./example

volume:
	docker volume create io-example

example: example-image volume
	docker run --ulimit nofile=5000:5000 -v io-example:/mnt/test -v /tmp:/tmp -it io-example /main-app

example-inject:debug-toda
	cat ./io-inject-example.json|sudo -E ./target/debug/toda --path /mnt/test --pid $$(pgrep main-app) --verbose trace

debug-toda:
	RUSTFLAGS="-Z relro-level=full" cargo build
