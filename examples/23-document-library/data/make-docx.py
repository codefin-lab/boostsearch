#!/usr/bin/env python3
"""Write the two Word files in data/files/, byte for byte the same every time.

They are committed, so nothing needs to run this. It is here so that what is
inside a .docx is not a mystery: a zip holding four XML parts -- the list of
parts, the relationship that says which part is the document, the document
body, and `docProps/core.xml` with the title, author, keywords and date the
attachment processor reads.

    python3 data/make-docx.py
"""
import os
import zipfile
from xml.sax.saxutils import escape

HERE = os.path.dirname(os.path.abspath(__file__))

CONTENT_TYPES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
    '<Default Extension="xml" ContentType="application/xml"/>'
    '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
    '<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>'
    "</Types>"
)

RELS = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
    '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>'
    '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>'
    "</Relationships>"
)


def document(paragraphs):
    body = "".join(f"<w:p><w:r><w:t>{escape(p)}</w:t></w:r></w:p>" for p in paragraphs)
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        f"<w:body>{body}</w:body></w:document>"
    )


def core(title, author, keywords, created):
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"'
        ' xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"'
        ' xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">'
        f"<dc:title>{escape(title)}</dc:title><dc:creator>{escape(author)}</dc:creator>"
        f"<cp:keywords>{escape(keywords)}</cp:keywords>"
        f'<dcterms:created xsi:type="dcterms:W3CDTF">{created}</dcterms:created>'
        "</cp:coreProperties>"
    )


def write(path, paragraphs, title, author, keywords, created):
    # a fixed timestamp on every entry, so the file does not change when rebuilt
    stamp = (2026, 1, 1, 0, 0, 0)
    with zipfile.ZipFile(os.path.join(HERE, path), "w", zipfile.ZIP_DEFLATED) as z:
        for name, text in [
            ("[Content_Types].xml", CONTENT_TYPES),
            ("_rels/.rels", RELS),
            ("word/document.xml", document(paragraphs)),
            ("docProps/core.xml", core(title, author, keywords, created)),
        ]:
            info = zipfile.ZipInfo(name, stamp)
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, text)


write(
    "files/it_handbook_2026-01-15_security-handbook.docx",
    [
        "Security handbook",
        "Every laptop is encrypted before it leaves the IT desk.",
        "Use a password manager, and never reuse a password between two services.",
        "A lost laptop or phone is reported to the security team within the hour.",
    ],
    "Security handbook",
    "Priya Nair",
    "security, laptops, passwords",
    "2026-01-15T09:00:00Z",
)

write(
    "files/board-pack/risk-register.docx",
    [
        "Risk register",
        "Currency exposure in the new markets. Owner: the finance director.",
        "A leaked password to the billing system. Owner: the security team.",
    ],
    "Risk register",
    "Tomasz Wojcik",
    "risks, board",
    "2026-02-03T14:30:00Z",
)
