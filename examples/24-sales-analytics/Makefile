INDICES = sales sales-monthly

.POSIX:
.PHONY: help run serve check clean data

help:
	@echo "make serve   start a node for this example (foreground, port 9284)"
	@echo "make run     run the example against it"
	@echo "make check   syntax and JSON checks, no server needed"
	@echo "make clean   delete what the example left in the server"
	@echo "make data    regenerate the sales lines (the same bytes every time)"

serve:
	./node.sh

run:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; BS=$${BS:-http://127.0.0.1:9284} ./run.sh'

check:
	@bash -n run.sh && echo "run.sh: syntax ok"
	@python3 -c "import glob,json,sys; \
[json.load(open(f)) for f in glob.glob('requests/*.json')]; \
[json.loads(l) for f in glob.glob('data/*.ndjson') for l in open(f) if l.strip()]; \
print('requests and data: valid JSON')"
	@python3 data/make-sales.py | cmp -s - data/01-a-year-of-sales-lines-one.ndjson \
&& echo "data: matches its generator" || { echo "data: differs from data/make-sales.py" >&2; exit 1; }

data:
	python3 data/make-sales.py > data/01-a-year-of-sales-lines-one.ndjson

clean:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; \
for i in $(INDICES); do curl -sS -X DELETE "$${BS:-http://127.0.0.1:9284}/$$i" > /dev/null 2>&1 || true; done; \
echo "deleted: $(INDICES)"'
