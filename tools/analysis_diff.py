#!/usr/bin/env python3
"""What OpenSearch makes of a text, and what BoostSearch makes of the same one.

Runs every built-in analyzer, tokenizer and filter the corpus names over a
handful of texts against both servers and reports where the token streams
differ. This is the gate for the analysis phase: the diff is meant to be
empty.
"""
import json, sys, urllib.request

import os
A = os.environ.get("DIFF_A", "http://127.0.0.1:9299")   # OpenSearch
B = os.environ.get("DIFF_B", "http://127.0.0.1:9200")   # BoostSearch

TEXTS = [
    "The quick brown foxes jumped over the lazy dogs",
    "Brown-Foxes don't jump.",
    "Musée d'Orsay, café & résumé",
    "test #test @test 3.14 foo_bar",
    "  spaced   out  ",
    "ABC abc ÄÖÜ ß",
]
ANALYZERS = [
    "standard", "simple", "whitespace", "stop", "keyword", "pattern", "fingerprint",
    "english", "french", "german", "spanish", "italian", "portuguese", "dutch",
    "danish", "swedish", "norwegian", "finnish", "russian", "hungarian", "romanian",
    "turkish", "arabic", "greek", "czech", "catalan", "basque", "irish", "latvian",
    "lithuanian", "estonian", "galician", "indonesian", "brazilian", "bulgarian",
    "hindi", "bengali", "persian", "sorani", "armenian", "thai", "cjk",
]
TOKENIZERS = [
    "standard", "keyword", "whitespace", "letter", "lowercase", "classic",
    "uax_url_email", "path_hierarchy", "ngram", "edge_ngram", "pattern",
]
FILTERS = [
    "lowercase", "uppercase", "asciifolding", "trim", "reverse", "unique", "stop",
    "porter_stem", "kstem", "fingerprint", "apostrophe", "classic", "decimal_digit",
    "cjk_width", "cjk_bigram", "word_delimiter", "arabic_normalization",
    "german_normalization", "hindi_normalization", "indic_normalization",
    "persian_normalization", "scandinavian_normalization", "scandinavian_folding",
    "serbian_normalization", "sorani_normalization", "bengali_normalization",
    "arabic_stem", "brazilian_stem", "czech_stem", "dutch_stem", "french_stem",
    "german_stem", "persian_stem", "russian_stem",
]

def tokens(base, body):
    req = urllib.request.Request(
        base + "/_analyze", data=json.dumps(body).encode(),
        method="POST", headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as answer:
            return [t["token"] for t in json.load(answer)["tokens"]]
    except Exception as e:
        return [f"<error {getattr(e, 'code', e)}>"]

def main():
    show = "-v" in sys.argv
    cases = []
    for text in TEXTS:
        for name in ANALYZERS:
            cases.append((f"analyzer:{name}", {"text": text, "analyzer": name}))
        for name in TOKENIZERS:
            cases.append((f"tokenizer:{name}", {"text": text, "tokenizer": name}))
        for name in FILTERS:
            cases.append((f"filter:{name}",
                          {"text": text, "tokenizer": "standard", "filter": [name]}))
    same = 0
    differences = []
    for label, body in cases:
        theirs, ours = tokens(A, body), tokens(B, body)
        if theirs == ours:
            same += 1
        else:
            differences.append((label, body["text"], theirs, ours))
    for label, text, theirs, ours in differences if show else []:
        print(f"{label:<34} {text!r}\n    OpenSearch  {theirs}\n    BoostSearch {ours}")
    print(f"\n{same} of {len(cases)} identical  ({100 * same / len(cases):.1f}%)")
    return 0 if not differences else 1

sys.exit(main())
