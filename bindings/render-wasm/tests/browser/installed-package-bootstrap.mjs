const result = document.querySelector("#result");

addEventListener("error", (event) => fail(event.error ?? event.message));
addEventListener("unhandledrejection", (event) => fail(event.reason));

try {
  await import("./installed-package.mjs");
} catch (error) {
  fail(error);
}

function fail(error) {
  result.textContent = `FAIL ${error?.code ?? error?.name ?? "error"}: ${
    error?.message ?? error
  }`;
  result.id = "fail";
  document.title = "FAIL";
}
