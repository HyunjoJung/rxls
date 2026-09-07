export function downloadBlob(blob, fileName, browser = globalThis) {
  const url = browser.URL.createObjectURL(blob);
  const anchor = browser.document.createElement("a");
  anchor.href = url;
  anchor.download = fileName;
  anchor.click();
  browser.setTimeout(() => browser.URL.revokeObjectURL(url), 1_000);
}

export function loadImage(url, browser = globalThis) {
  return new Promise((resolve, reject) => {
    const image = new browser.Image();
    image.addEventListener("load", () => resolve(image), { once: true });
    image.addEventListener("error", () => reject(new Error("SVG rasterization failed.")), {
      once: true
    });
    image.src = url;
  });
}
