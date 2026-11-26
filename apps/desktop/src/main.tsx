import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { DownloadCompleteWindow } from "./windows/DownloadCompleteWindow";
import { DownloadConfirmWindow } from "./windows/DownloadConfirmWindow";
import { DownloadProgressWindow } from "./windows/DownloadProgressWindow";
import "./styles/app.css";

function Root() {
  const params = new URLSearchParams(window.location.search);
  const windowKind = params.get("window");
  const id = params.get("id")?.trim();

  if (windowKind === "confirm" && id) {
    return <DownloadConfirmWindow pendingId={id} />;
  }
  if (windowKind === "progress" && id) {
    return <DownloadProgressWindow transferId={id} />;
  }
  if (windowKind === "complete" && id) {
    return <DownloadCompleteWindow transferId={id} />;
  }
  return <App />;
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Root />
  </StrictMode>,
);
