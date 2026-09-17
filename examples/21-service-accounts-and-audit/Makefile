INDICES = orders-2026.09 payroll
SECURITY = internalusers/svc-ingest internalusers/dash-viewer rolesmapping/ingest_orders rolesmapping/dashboard_ro roles/ingest_orders roles/dashboard_ro actiongroups/orders_writer tenants/ops_dashboards

.POSIX:
.PHONY: help run serve check clean

help:
	@echo "make serve   start a node for this example (foreground, port 9281)"
	@echo "make run     run the example against it"
	@echo "make check   syntax and JSON checks, no server needed"
	@echo "make clean   delete what the example left in the server"

serve:
	./node.sh

run:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; VS="$${VS:-http://127.0.0.1:9281}" ./run.sh'

check:
	@bash -n run.sh && echo "run.sh: syntax ok"
	@python3 -c "import glob,json,sys; \
[json.load(open(f)) for f in glob.glob('requests/*.json')]; \
[json.loads(l) for f in glob.glob('data/*.ndjson') for l in open(f) if l.strip()]; \
print('requests and data: valid JSON')"

clean:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; bs=$${VS:-http://127.0.0.1:9281}; auth=$${AUTH:-admin:Example-Passphrase-2026}; \
for i in $(INDICES); do curl -sS -u "$$auth" -X DELETE "$$bs/$$i" > /dev/null 2>&1 || true; done; \
for p in $(SECURITY); do curl -sS -u "$$auth" -X DELETE "$$bs/_plugins/_security/api/$$p" > /dev/null 2>&1 || true; done; \
echo "deleted: $(INDICES) $(SECURITY)"'
