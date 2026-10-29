// This runtime runs only in the existing opaque Input sandbox.
(() => {
  let config;
  let images = [];
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
