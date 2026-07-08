"""Sphinx configuration for the rtrash project site (Shibuya theme)."""

from __future__ import annotations

import os
import sys
from pathlib import Path

# Repo root on sys.path only if we ever autodoc Rust via other means.
_DOCS = Path(__file__).resolve().parent
_ROOT = _DOCS.parent.parent

project = "rtrash"
copyright = "2026, Rohit Goswami"
author = "Rohit Goswami"
release = "0.1.3"
version = "0.1"

extensions = [
    "sphinx.ext.autodoc",
    "sphinx_copybutton",
    "sphinx_design",
]

templates_path = ["_templates"]
exclude_patterns: list[str] = []

html_theme = "shibuya"
html_static_path = ["_static"]
html_favicon = "_static/favicon.svg"
html_logo = "_static/logo.svg"
html_title = "rtrash"
html_css_files = ["custom.css"]

html_context = {
    "source_type": "github",
    "source_user": "HaoZeke",
    "source_repo": "rtrash",
    "source_version": "main",
    "source_docs_path": "/docs/source/",
}

html_theme_options = {
    "accent_color": "teal",
    # Wordmark variants: light nav uses html_logo; dark uses logo_dark when theme supports it.
    "light_logo": "_static/logo.svg",
    "dark_logo": "_static/logo-dark.svg",
    "github_url": "https://github.com/HaoZeke/rtrash",
    "nav_links": [
        {"title": "Get started", "url": "getting-started"},
        {"title": "Architecture", "url": "architecture"},
        {"title": "Benchmarks", "url": "benchmarks"},
        {"title": "Python", "url": "bindings"},
    ],
}

# Avoid requiring a network for intersphinx during offline builds.
intersphinx_mapping: dict = {}

# Copybutton: skip prompts in console blocks
copybutton_prompt_text = r">>> |\.\.\. |\$ |In \[\d*\]: | {2,5}\.\.\.: | {5,8}: "
copybutton_prompt_is_regexp = True
