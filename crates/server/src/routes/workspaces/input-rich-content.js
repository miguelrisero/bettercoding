// This runtime runs only in the existing opaque Input sandbox.
(() => {
  let config;
  let images = [];
  const linkActivated = (event) => {
    if (event.type === "auxclick" && event.button !== 1) return;
    const target = event.target?.closest
      ? event.target
      : event.target?.parentElement;
    const anchor = target?.closest("a[href], area[href], a[*|href]");
    if (!anchor) return;
    const href =
      anchor.getAttribute("href") ?? anchor.getAttribute("xlink:href") ?? "";
    if (anchor.getAttribute("xlink:href") != null)
      anchor.removeAttribute("xlink:href");
    if (
      !event.isTrusted ||
      href.length > 8192 ||
      /[\s\\\u0000-\u001f\u007f]/.test(href)
    ) {
      event.preventDefault();
      return;
    }
    if (href.startsWith("#")) {
      anchor.setAttribute("href", href);
      anchor.setAttribute("target", "_self");
      return;
    }
    try {
      const url = new URL(href);
      if (
        !/^https?:\/\//i.test(href) ||
        !/^https?:$/.test(url.protocol) ||
        !url.hostname ||
        url.username ||
        url.password ||
        url.href.length > 8192
      ) {
        event.preventDefault();
        return;
      }
      anchor.setAttribute("href", url.href);
      anchor.setAttribute("target", "_blank");
      anchor.setAttribute("rel", "noopener noreferrer");
    } catch {
      event.preventDefault();
    }
  };
  document.addEventListener("click", linkActivated, true);
  document.addEventListener("auxclick", linkActivated, true);
  window.addEventListener("message", async (event) => {
    if (event.source !== parent) return;
    const data = event.data;
    if (!data || data.protocol !== 1) return;
    if (
      data.type === "bc.input.image" &&
      config &&
      data.bridge_id === config.bridge_id &&
      data.input_id === config.input_id &&
      data.version === config.version &&
      Number.isInteger(data.index)
    ) {
      const image = images[data.index];
      if (
        image &&
        /^data:image\/(png|jpeg|gif|webp|avif|bmp|x-icon);base64,[a-zA-Z0-9+/=]+$/.test(
          data.src,
        )
      )
        image.src = data.src;
      return;
    }
    if (data.type !== "bc.input.init" || config) return;
    config = data;
    images = Array.from(document.querySelectorAll("img"));
    images.forEach((image, index) => {
      const src = image.getAttribute("src") || "";
      image.removeAttribute("srcset");
      if (
        /^data:image\/(png|jpeg|gif|webp|avif|bmp|x-icon);base64,[a-zA-Z0-9+/=\s]+$/.test(
          src,
        )
      )
        return;
      image.removeAttribute("src");
      parent.postMessage(
        {
          type: "bc.input.image.request",
          protocol: 1,
          bridge_id: config.bridge_id,
          input_id: config.input_id,
          version: config.version,
          index,
          src,
        },
        "*",
      );
    });
    if (typeof data.mermaid_source !== "string") return;
    try {
      const script = document.createElement("script");
      script.textContent = data.mermaid_source;
      document.head.append(script);
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        suppressErrorRendering: true,
        flowchart: { htmlLabels: false },
      });
      const blocks = Array.from(
        document.querySelectorAll(
          "pre.mermaid, pre:has(> code.language-mermaid), pre",
        ),
      );
      for (const [index, block] of blocks.entries()) {
        const text = block.textContent || "";
        const fenced = /^\s*```mermaid\s*\n([\s\S]*?)\n```\s*$/.exec(text);
        if (
          !block.matches("pre.mermaid, pre:has(> code.language-mermaid)") &&
          !fenced
        )
          continue;
        const chart = fenced ? fenced[1] : text;
        try {
          const { svg } = await mermaid.render(`bc-mermaid-${index}`, chart);
          // Mermaid's strict renderer sanitizes SVG; Input DOM stays opaque.
          block.innerHTML = svg;
          block.setAttribute("aria-label", chart);
        } catch {
          // Keep the exact source available when a diagram cannot render.
        }
      }
    } catch {
      // A missing renderer leaves readable source and the proposal bridge intact.
    }
  });
})();
