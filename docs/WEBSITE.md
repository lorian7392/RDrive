# Documentation website

The RDrive documentation site is published at **[lorian7392.github.io/RDrive](https://lorian7392.github.io/RDrive/)**. Its source lives in `docs/`, with navigation and theme settings in the repository's `mkdocs.yml`.

The site uses MkDocs and Material, with English navigation, full-text documentation search, light and dark themes, page contents, and copyable code examples. Markdown remains available in Git for review and offline reading; GitHub Pages serves the built HTML site.

## Preview locally

Use Python 3.11 or later from the repository root:

```sh
python3 -m venv target/docs-venv
target/docs-venv/bin/python -m pip install -r docs/requirements.txt
target/docs-venv/bin/python -m mkdocs serve
```

Open the local URL printed by MkDocs. The project site uses `/RDrive/` as its base path, including in local previews.

Build exactly as the documentation workflow does:

```sh
target/docs-venv/bin/python -m unittest discover -s tests -p 'test_docs_site.py' -v
target/docs-venv/bin/python -m mkdocs build --strict
RDRIVE_DOCS_SITE=target/docs-site \
  target/docs-venv/bin/python -m unittest discover -s tests -p 'test_docs_site.py' -v
```

The output is `target/docs-site/`. `--strict` rejects broken navigation, document links, and anchors. The site tests check English prose, the publication scope, expected pages, search output, and internal HTML links.

## Edit a page

1. Edit its Markdown source in `docs/`. Keep prose in English and examples independent of personal accounts, paths, and active storage.
2. Add new pages to `mkdocs.yml`. Link to another documentation source with a relative `.md` path; link to repository files outside `docs/` with their GitHub URL.
3. Regenerate CLI or MCP reference pages after changing their interfaces. The generator commands are recorded at the top of each reference page.
4. Run the strict build and documentation tests. Changes to code examples should also run the relevant CLI or protocol tests.

The theme uses system fonts and bundles its assets; no analytics are configured. Keep application implementation details in the architecture or development guides rather than in introductory task flows.

## Publication

The **Documentation** workflow validates pull requests targeting `main`. On pushes to `main`, it builds an artifact and deploys that artifact through GitHub Pages using the `github-pages` environment. A manual workflow run on `main` can redeploy the current site. Pull requests build the site without publishing it.

The build job needs read access to the repository. Only the deployment job receives `pages: write` and `id-token: write`; the workflow does not push generated HTML into the source branch. The repository's Pages publishing source is GitHub Actions.

Historical validation Markdown, raw environment reports, and the native deployment archive remain in the repository and are excluded from the published site. Links to those records open GitHub. The public coverage summary and its compact, per-file report are included. Do not place credentials or private runtime data in the documentation tree.

For CLI build and test workflows and source version tags, see [Version tags and CI builds](RELEASING.md). A successful documentation deployment is independent of application build and test results.
