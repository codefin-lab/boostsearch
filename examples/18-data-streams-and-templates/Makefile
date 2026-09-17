STREAMS = metrics-node-cpu metrics-edge-cpu
INDICES = legacy-metrics-000001 legacy-metrics-000002
TEMPLATES = metrics-edge metrics

.POSIX:
.PHONY: help run serve check clean

help:
	@echo "make serve   start a node for this example (foreground, port 9278)"
	@echo "make run     run the example against it"
	@echo "make check   syntax and JSON checks, no server needed"
	@echo "make clean   delete what the example left in the server"

serve:
	./node.sh

run:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; VS="$${VS:-http://127.0.0.1:9278}" ./run.sh'

check:
	@bash -n run.sh && echo "run.sh: syntax ok"
	@python3 -c "import glob,json,sys; \
[json.load(open(f)) for f in glob.glob('requests/*.json')]; \
[json.loads(l) for f in glob.glob('data/*.ndjson') for l in open(f) if l.strip()]; \
print('requests and data: valid JSON')"

clean:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; VS=$${VS:-http://127.0.0.1:9278}; \
for s in $(STREAMS); do curl -sS -X DELETE "$$VS/_data_stream/$$s" > /dev/null 2>&1 || true; done; \
for i in $(INDICES); do curl -sS -X DELETE "$$VS/$$i" > /dev/null 2>&1 || true; done; \
for t in $(TEMPLATES); do curl -sS -X DELETE "$$VS/_index_template/$$t" > /dev/null 2>&1 || true; done; \
curl -sS -X DELETE "$$VS/_template/legacy-metrics" > /dev/null 2>&1 || true; \
for c in metrics-settings metrics-mappings; do curl -sS -X DELETE "$$VS/_component_template/$$c" > /dev/null 2>&1 || true; done; \
echo "deleted: streams $(STREAMS), indices $(INDICES), their templates"'
