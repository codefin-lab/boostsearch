INDICES = library
PIPELINES = library library-extract library-final library-bundle

.POSIX:
.PHONY: help run serve check clean

help:
	@echo "make serve   start a node for this example (foreground, port 9283)"
	@echo "make run     run the example against it"
	@echo "make check   syntax and JSON checks, no server needed"
	@echo "make clean   delete what the example left in the server"

serve:
	./node.sh

run:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; BS="$${BS:-http://127.0.0.1:9283}" ./run.sh'

check:
	@bash -n run.sh && echo "run.sh: syntax ok"
	@python3 -c "import glob,json,sys; \
[json.load(open(f)) for f in glob.glob('requests/*.json')]; \
[json.loads(l) for f in glob.glob('data/*.ndjson') for l in open(f) if l.strip()]; \
print('requests and data: valid JSON')"
	@python3 -c "import glob,zipfile,ast; \
ast.parse(open('data/make-docx.py').read()); \
txt = glob.glob('data/files/**/*.txt', recursive=True); \
docx = glob.glob('data/files/**/*.docx', recursive=True); \
[open(f, encoding='utf-8').read() for f in txt]; \
[zipfile.ZipFile(f).read(p) for f in docx for p in ('word/document.xml', 'docProps/core.xml')]; \
assert len(txt) == 4 and len(docx) == 2, (txt, docx); \
print('data/files: %d text files readable, %d Word files hold a document and its properties' % (len(txt), len(docx)))"

clean:
	@sh -c 'if [ -f .env ]; then . ./.env; fi; \
for i in $(INDICES); do curl -sS -X DELETE "$${BS:-http://127.0.0.1:9283}/$$i" > /dev/null 2>&1 || true; done; \
for p in $(PIPELINES); do curl -sS -X DELETE "$${BS:-http://127.0.0.1:9283}/_ingest/pipeline/$$p" > /dev/null 2>&1 || true; done; \
echo "deleted: $(INDICES), and the pipelines $(PIPELINES)"'
