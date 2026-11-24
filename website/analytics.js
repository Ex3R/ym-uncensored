(() => {
  const counter = Number(document.currentScript.dataset.counter);
  if (location.hostname !== "ym.idkroma.ru" || !Number.isSafeInteger(counter) || counter <= 0) return;

  const ym = window.ym ||= function () { (ym.a ||= []).push(arguments); };
  ym.l = Date.now();
  ym(counter, "init", { webvisor: false, clickmap: false, trackLinks: true });
  const tag = document.createElement("script");
  tag.async = true;
  tag.src = "https://mc.yandex.ru/metrika/tag.js";
  document.head.append(tag);

  document.addEventListener("click", event => {
    const el = event.target.closest("button, a");
    if (!el) return;
    if (el.matches(".file")) {
      ym(counter, "reachGoal", "download_click", { platform: el.dataset.os });
    } else if (el.matches("[data-mix]")) {
      ym(counter, "reachGoal", "compare_click", { version: el.dataset.mix === "0" ? "clean" : "original" });
    } else if (el.matches('a[href^="https://pay.cloudtips.ru/"]')) {
      ym(counter, "reachGoal", "donate_click");
    }
  });
})();
