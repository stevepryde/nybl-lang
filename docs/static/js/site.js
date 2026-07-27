(() => {
  const root = document.documentElement;
  const themeColor = document.querySelector('meta[name="theme-color"]');
  const themeButtons = document.querySelectorAll("[data-theme-toggle]");

  function setTheme(theme, persist = true) {
    root.dataset.theme = theme;
    if (themeColor) {
      themeColor.content = theme === "dark" ? "#111318" : "#f6f2e9";
    }
    themeButtons.forEach((button) => {
      button.setAttribute(
        "aria-label",
        theme === "dark" ? "Switch to light theme" : "Switch to dark theme",
      );
    });
    if (persist) localStorage.setItem("nybl-theme", theme);
  }

  setTheme(root.dataset.theme || "dark", false);
  themeButtons.forEach((button) => {
    button.addEventListener("click", () => {
      setTheme(root.dataset.theme === "dark" ? "light" : "dark");
    });
  });

  const header = document.querySelector("[data-site-header]");
  const updateHeader = () => header?.classList.toggle("is-scrolled", scrollY > 8);
  updateHeader();
  addEventListener("scroll", updateHeader, { passive: true });

  const siteMenu = document.querySelector("[data-site-menu]");
  const siteMenuToggle = document.querySelector("[data-site-menu-toggle]");

  function setSiteMenu(open) {
    if (!siteMenu || !siteMenuToggle) return;
    siteMenu.hidden = !open;
    siteMenuToggle.setAttribute("aria-expanded", String(open));
    siteMenuToggle.querySelector(".sr-only").textContent = open
      ? "Close navigation"
      : "Open navigation";
  }

  siteMenuToggle?.addEventListener("click", () => {
    setSiteMenu(siteMenu?.hidden ?? true);
  });

  document.querySelectorAll("[data-copy]").forEach((button) => {
    button.addEventListener("click", async () => {
      const value = button.dataset.copy;
      const label = button.querySelector("[data-copy-label]");
      if (!value) return;

      try {
        await navigator.clipboard.writeText(value);
        if (label) label.textContent = "Copied";
        button.classList.add("is-copied");
        setTimeout(() => {
          if (label) label.textContent = "Copy";
          button.classList.remove("is-copied");
        }, 1800);
      } catch {
        if (label) label.textContent = "Select command";
        const code = button.querySelector("code");
        const selection = getSelection();
        if (code && selection) {
          const range = document.createRange();
          range.selectNodeContents(code);
          selection.removeAllRanges();
          selection.addRange(range);
        }
      }
    });
  });

  const docsDrawer = document.querySelector("[data-docs-drawer]");
  const docsBackdrop = document.querySelector("[data-docs-drawer-backdrop]");
  const docsOpen = document.querySelector("[data-docs-menu-open]");
  const docsClose = document.querySelector("[data-docs-menu-close]");
  let drawerReturnFocus = null;

  function setDocsDrawer(open) {
    if (!docsDrawer || !docsBackdrop) return;
    docsDrawer.classList.toggle("is-open", open);
    docsDrawer.setAttribute("aria-hidden", String(!open));
    docsBackdrop.hidden = !open;
    docsOpen?.setAttribute("aria-expanded", String(open));
    document.body.style.overflow = open ? "hidden" : "";

    if (open) {
      drawerReturnFocus = document.activeElement;
      setTimeout(() => docsClose?.focus(), 180);
    } else if (drawerReturnFocus instanceof HTMLElement) {
      drawerReturnFocus.focus();
    }
  }

  docsOpen?.addEventListener("click", () => setDocsDrawer(true));
  docsClose?.addEventListener("click", () => setDocsDrawer(false));
  docsBackdrop?.addEventListener("click", () => setDocsDrawer(false));
  docsDrawer?.querySelectorAll("a").forEach((link) => {
    link.addEventListener("click", () => setDocsDrawer(false));
  });

  const searchDialog = document.querySelector("[data-search-dialog]");
  const searchInput = document.querySelector("[data-search-input]");
  const searchResults = document.querySelector("[data-search-results]");
  const searchStatus = document.querySelector("[data-search-status]");
  let searchIndex = null;
  let searchReturnFocus = null;

  const plainText = (value = "") =>
    value
      .replace(/<[^>]+>/g, " ")
      .replace(/[*_`~\[\]]/g, "")
      .replace(/\s+/g, " ")
      .trim();

  async function loadSearchIndex() {
    if (searchIndex) return searchIndex;
    if (searchStatus) searchStatus.textContent = "Loading the documentation index…";

    try {
      const response = await fetch("/search_index.en.json");
      if (!response.ok) throw new Error(`Search index returned ${response.status}`);
      const payload = await response.json();
      searchIndex = (Array.isArray(payload) ? payload : payload.items || []).filter(
        (item) => item.title,
      );
      if (searchStatus) {
        searchStatus.textContent = "Start typing to search the guide and reference.";
      }
    } catch {
      searchIndex = [];
      if (searchStatus) {
        searchStatus.textContent =
          "Search is unavailable right now. The documentation navigation is still available.";
      }
    }
    return searchIndex;
  }

  function scoreDocument(document, terms) {
    const title = plainText(document.title).toLowerCase();
    const description = plainText(document.description).toLowerCase();
    const content = plainText(document.body || document.content).toLowerCase();
    const path = (document.path || document.url || document.permalink || "").toLowerCase();
    let score = 0;

    for (const term of terms) {
      let found = false;
      if (title === term) {
        score += 30;
        found = true;
      } else if (title.startsWith(term)) {
        score += 18;
        found = true;
      } else if (title.includes(term)) {
        score += 12;
        found = true;
      }
      if (description.includes(term)) {
        score += 5;
        found = true;
      }
      if (path.includes(term)) {
        score += 3;
        found = true;
      }
      if (content.includes(term)) {
        score += 1;
        found = true;
      }
      if (!found) return -1;
    }
    return score;
  }

  function renderSearchResults(query) {
    if (!searchResults || !searchStatus || !searchIndex) return;
    searchResults.replaceChildren();
    const terms = query.toLowerCase().trim().split(/\s+/).filter(Boolean);

    if (!terms.length) {
      searchStatus.textContent = "Start typing to search the guide and reference.";
      return;
    }

    const matches = searchIndex
      .map((document) => ({ document, score: scoreDocument(document, terms) }))
      .filter((match) => match.score >= 0)
      .sort((a, b) => b.score - a.score)
      .slice(0, 8);

    searchStatus.textContent = matches.length
      ? `${matches.length} best ${matches.length === 1 ? "match" : "matches"}`
      : `No results for “${query.trim()}”`;

    for (const { document: entry } of matches) {
      const item = document.createElement("li");
      const link = document.createElement("a");
      const title = document.createElement("strong");
      const excerpt = document.createElement("p");
      const path = document.createElement("span");
      const href = entry.url || entry.permalink || entry.path || "/docs/";

      link.href = href;
      title.textContent = plainText(entry.title) || "Untitled";
      excerpt.textContent =
        plainText(entry.description || entry.body || entry.content).slice(0, 180) ||
        "Open this documentation page.";
      path.textContent = new URL(href, location.origin).pathname;
      link.append(title, excerpt, path);
      item.append(link);
      searchResults.append(item);
    }
  }

  async function openSearch() {
    if (!(searchDialog instanceof HTMLDialogElement)) return;
    searchReturnFocus = document.activeElement;
    if (!searchDialog.open) searchDialog.showModal();
    await loadSearchIndex();
    searchInput?.focus();
  }

  function closeSearch() {
    if (!(searchDialog instanceof HTMLDialogElement) || !searchDialog.open) return;
    searchDialog.close();
    if (searchReturnFocus instanceof HTMLElement) searchReturnFocus.focus();
  }

  document.querySelectorAll("[data-search-open]").forEach((button) => {
    button.addEventListener("click", openSearch);
  });
  document
    .querySelector("[data-search-close]")
    ?.addEventListener("click", closeSearch);

  searchDialog?.addEventListener("click", (event) => {
    if (event.target === searchDialog) closeSearch();
  });

  searchInput?.addEventListener("input", (event) => {
    renderSearchResults(event.currentTarget.value);
  });

  addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      openSearch();
    }
    if (event.key === "Escape" && docsDrawer?.classList.contains("is-open")) {
      setDocsDrawer(false);
    }
  });
})();
