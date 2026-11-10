"""Documentation publication checks, using only the Python standard library."""
from html.parser import HTMLParser
import os
from pathlib import Path
import re
import unittest
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]


def published_pages():
    configuration = (ROOT / "mkdocs.yml").read_text()
    return [ROOT / "docs" / name for name in re.findall(r"^\s+- [^:\n]+: ([A-Za-z][A-Za-z0-9_/.-]*\.md)$", configuration, re.MULTILINE)]


def prose(markdown):
    text = re.sub(r"(?ms)^```.*?^```[^\n]*$", "", markdown)
    return re.sub(r"`[^`\n]*`", "", text)


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.links, self.anchors, self.language = [], set(), None
        self.feed(text)

    def handle_starttag(self, tag, attributes):
        attributes = dict(attributes)
        if tag == "html":
            self.language = attributes.get("lang")
        if "id" in attributes:
            self.anchors.add(attributes["id"])
        if tag == "a" and attributes.get("href"):
            self.links.append(attributes["href"])


class DocumentationSiteTests(unittest.TestCase):
    def test_maintained_documentation_uses_english_prose(self):
        pages = published_pages()
        self.assertGreaterEqual(len(pages), 20)
        pages += [ROOT / name for name in ["README.md", "CONTRIBUTING.md", "SECURITY.md", "CHANGELOG.md"]]
        for page in pages:
            with self.subTest(page=str(page.relative_to(ROOT))):
                self.assertTrue(page.is_file())
                self.assertIsNone(re.search(r"[\u3400-\u9fff]", prose(page.read_text())), "English prose required; Unicode filename examples may remain in code")

    def test_pages_workflow_only_publishes_main_and_has_scoped_permissions(self):
        workflow = (ROOT / ".github/workflows/docs.yml").read_text()
        self.assertIn("mkdocs build --strict", workflow)
        self.assertIn("github.ref == 'refs/heads/main'", workflow)
        self.assertIn("github.event_name != 'pull_request'", workflow)
        self.assertIn("needs: build", workflow)
        self.assertIn("pages: write", workflow)
        self.assertIn("id-token: write", workflow)
        self.assertIn("path: target/docs-site", workflow)
        self.assertNotIn("gh-deploy", workflow)

    @unittest.skipUnless(os.environ.get("RDRIVE_DOCS_SITE"), "set RDRIVE_DOCS_SITE after the strict site build")
    def test_generated_site_has_search_english_pages_and_no_archived_reports(self):
        site = (ROOT / os.environ["RDRIVE_DOCS_SITE"]).resolve()
        self.assertTrue((site / "index.html").is_file())
        self.assertTrue((site / "search/search_index.json").is_file())
        self.assertFalse((site / "archive").exists())
        self.assertFalse(list(site.glob("*VALIDATION*.json")))
        self.assertFalse(list(site.glob("NATIVE_ACTIVATION*.json")))
        self.assertFalse(list(site.glob("VALIDATION*")))
        self.assertFalse(any(path.is_symlink() for path in site.rglob("*")))
        for source in published_pages():
            relative = source.relative_to(ROOT / "docs")
            target = site / ("index.html" if relative.name == "README.md" else str(relative.with_suffix("")) + "/index.html")
            with self.subTest(page=str(relative)):
                self.assertTrue(target.is_file(), target)
                self.assertEqual(Page(target.read_text()).language, "en")

    @unittest.skipUnless(os.environ.get("RDRIVE_DOCS_SITE"), "set RDRIVE_DOCS_SITE after the strict site build")
    def test_generated_internal_links_and_anchors_resolve(self):
        site = (ROOT / os.environ["RDRIVE_DOCS_SITE"]).resolve()
        pages = {path.resolve(): Page(path.read_text()) for path in site.rglob("*.html")}
        for source, page in pages.items():
            for link in page.links:
                parsed = urlsplit(link)
                if parsed.scheme or parsed.netloc:
                    continue
                path = unquote(parsed.path)
                if path.startswith("/RDrive/"):
                    target = site / path.removeprefix("/RDrive/")
                elif path.startswith("/"):
                    target = site / path.lstrip("/")
                else:
                    target = source.parent / path if path else source
                if target.is_dir():
                    target /= "index.html"
                target = target.resolve()
                with self.subTest(source=str(source.relative_to(site)), link=link):
                    self.assertTrue(target.is_relative_to(site), "internal link escapes the site base")
                    self.assertTrue(target.is_file(), str(target))
                    if parsed.fragment and target in pages:
                        self.assertIn(unquote(parsed.fragment), pages[target].anchors)


if __name__ == "__main__":
    unittest.main()
