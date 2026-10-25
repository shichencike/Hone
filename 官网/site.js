/* Hone 官网交互（原生 JS，无依赖，兼容 Android 8.1 / Chrome 61）
   - 主题初始化 + 切换（localStorage 防闪烁）
   - 移动端侧栏抽屉开合（汉堡按钮 / 遮罩 / 点击链接自动关闭） */
(function () {
  "use strict";

  /* ---------- 主题 ---------- */
  function applyTheme(t) {
    document.documentElement.setAttribute("data-theme", t);
  }
  function initTheme() {
    var t = "dark";
    try { t = localStorage.getItem("hone-theme") || "dark"; } catch (e) {}
    applyTheme(t);
  }
  function toggleTheme() {
    var cur = document.documentElement.getAttribute("data-theme");
    var next = cur === "light" ? "dark" : "light";
    applyTheme(next);
    try { localStorage.setItem("hone-theme", next); } catch (e) {}
  }
  initTheme();
  // 暴露给 header 按钮（onclick="toggleTheme()"）
  window.toggleTheme = toggleTheme;

  /* ---------- 抽屉 ---------- */
  function openNav() { document.body.className = document.body.className.replace(/\s*nav-open\s*/, "") + " nav-open"; }
  function closeNav() { document.body.className = document.body.className.replace(/\s*nav-open\s*/, ""); }
  window.toggleNav = function () {
    if (document.body.className.indexOf("nav-open") === -1) { openNav(); } else { closeNav(); }
  };
  window.closeNav = closeNav;

  // 点击遮罩关闭
  var backdrop = document.getElementById("navBackdrop");
  if (backdrop) {
    backdrop.onclick = function () { closeNav(); };
  }
  // 点击侧栏链接后自动关闭（移动端）
  var side = document.getElementById("sidebar");
  if (side) {
    var links = side.getElementsByTagName("a");
    for (var i = 0; i < links.length; i++) {
      links[i].onclick = function () { closeNav(); };
    }
  }
  // 按 Esc 关闭
  document.onkeydown = function (e) {
    if (e && e.key === "Escape") { closeNav(); }
  };
})();
