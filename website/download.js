const dialog = document.querySelector("dialog");
const retry = document.querySelector("#retry");
const statusLabel = document.querySelector("#download-status");
document.querySelectorAll(".file").forEach(link => {
  link.addEventListener("click", event => {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    if (typeof dialog.showModal !== "function") return;
    retry.href = link.href;
    statusLabel.textContent = "Скачивание для " + link.dataset.os;
    dialog.showModal();
    // The link's native action downloads the attachment, even when the donation is skipped.
  });
});
dialog.querySelectorAll("button").forEach(button => button.addEventListener("click", () => dialog.close()));
