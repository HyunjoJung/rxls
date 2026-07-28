export const NETWORK_NEGATIVE_URL =
  "https://rxls-network-negative.invalid/render-worker-control";

export function validateCspNetworkSilence({ proof, requests, responses }) {
  if (
    proof?.schema !== "rxls.render-csp-negative.v1" ||
    proof.fetchRejected !== true ||
    typeof proof.url !== "string"
  ) {
    throw new Error("CSP page proof is invalid");
  }
  if (
    requests.some(({ url }) => url === proof.url) ||
    responses.some(({ url }) => url === proof.url)
  ) {
    throw new Error("CSP control escaped into the CDP Network request pipeline");
  }
  return { url: proof.url, networkRequestEmitted: false };
}

export function validateOfflineNetworkBlock(
  { requests, failures, responses, pauses },
  controlUrl = NETWORK_NEGATIVE_URL
) {
  let parsedControlUrl;
  try {
    parsedControlUrl = new URL(controlUrl);
  } catch {
    throw new Error("CDP Network control URL is invalid");
  }
  if (parsedControlUrl.protocol !== "http:" && parsedControlUrl.protocol !== "https:") {
    throw new Error("CDP Network control URL has an invalid scheme");
  }
  const matchingPauses = pauses.filter(({ url }) => url === controlUrl);
  if (
    matchingPauses.length !== 1 ||
    matchingPauses[0].networkId === null ||
    typeof matchingPauses[0].sessionId !== "string" ||
    matchingPauses[0].resourceType !== "XHR"
  ) {
    throw new Error("CDP Fetch did not safely intercept the exact network control");
  }
  const matchingRequests = requests.filter(({ url }) => url === controlUrl);
  if (matchingRequests.length !== 1) {
    throw new Error(
      `CDP Network observed ${matchingRequests.length} offline controls instead of one`
    );
  }
  const [{ requestId, sessionId }] = matchingRequests;
  if (
    matchingPauses[0].networkId !== requestId ||
    matchingPauses[0].sessionId !== sessionId
  ) {
    throw new Error("CDP Fetch and Network request identities do not match");
  }
  const matchingFailures = failures.filter((failure) => failure.requestId === requestId);
  if (
    matchingFailures.length !== 1 ||
    matchingFailures[0].sessionId !== sessionId ||
    matchingFailures[0].blockedReason !== null ||
    matchingFailures[0].errorText !== "net::ERR_INTERNET_DISCONNECTED" ||
    matchingFailures[0].canceled !== false
  ) {
    throw new Error("CDP Network did not report the exact offline rejection");
  }
  if (responses.some((response) => response.requestId === requestId)) {
    throw new Error("Network negative control unexpectedly received a response");
  }
  return {
    url: controlUrl,
    requestId,
    errorText: "net::ERR_INTERNET_DISCONNECTED",
    responseReceived: false,
    interceptedBeforeResponse: true
  };
}
