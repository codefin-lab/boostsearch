INDICES = shop-products

.POSIX:
.PHONY: help run serve check clean

help:
	@echo "make serve   start a node for this example (foreground, port 9261)"
	@echo "make run     run the example against it"
	@echo "make check   syntax and JSON checks, no server needed"
	@echo "make clean   delete what the example left in the server"

serve:
	./node.sh

run:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; VS="$${VS:-http://127.0.0.1:9261}" ./run.sh'

check:
	@bash -n run.sh && echo "run.sh: syntax ok"
	@python3 -c "import glob,json,sys; \
[json.load(open(f)) for f in glob.glob('requests/*.json')]; \
[json.loads(l) for f in glob.glob('data/*.ndjson') for l in open(f) if l.strip()]; \
print('requests and data: valid JSON')"

clean:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; \
for i in $(INDICES); do curl -sS -X DELETE "$${VS:-http://127.0.0.1:9261}/$$i" > /dev/null 2>&1 || true; done; \
echo "deleted: $(INDICES)"'
