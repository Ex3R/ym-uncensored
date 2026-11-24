const dialog = document.querySelector("dialog");
const retry = document.querySelector("#retry");
const statusLabel = document.querySelector("#download-status");
document.querySelectorAll(".file").forEach(link => {
  link.addEventListener("click", event => {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    if (typeof dialog.showModal !== "function") return;
    retry.href = link.href;
    statusLabel.textContent = "Скачивание началось · " + link.dataset.os;
    dialog.showModal();
    // The link's native action downloads the attachment, even when the donation is skipped.
  });
});
dialog.querySelectorAll("button").forEach(button => button.addEventListener("click", () => dialog.close()));

const previews = {
  lyrics: ["assets/lyrics.webp", "Экран плеера с обложкой альбома и синхронным текстом песни"],
  album: ["assets/album.webp", "Альбом со списком треков и кнопкой воспроизведения"],
};
const screen = document.querySelector("#screen");
const modes = document.querySelectorAll("[data-view]");
modes.forEach(button => button.addEventListener("click", () => {
  [screen.src, screen.alt] = previews[button.dataset.view];
  modes.forEach(other => other.setAttribute("aria-pressed", String(other === button)));
}));
