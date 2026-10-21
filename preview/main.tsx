import "./mock-ipc";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "../src/App";
import PopoverApp from "../src/PopoverApp";
import "../src/styles.css";

const previewParams = new URLSearchParams(window.location.search);

// 菜单栏面板预览：真实环境里它是 352×560 的独立透明窗口，
// 这里用同样尺寸套一层底色，才能看出圆角与外描边的观感。
const isPopoverPreview = previewParams.get("pop") === "1";

if (isPopoverPreview) {
  document.body.style.background = "#e9e6e0";
  const root = document.getElementById("root") as HTMLElement;
  // 高度不写死：真实面板就是随内容变高的，预览跟着内容走才对得上
  root.style.cssText =
    "width:352px;margin:32px auto;border-radius:14px;" +
    "overflow:hidden;box-shadow:0 12px 32px rgba(0,0,0,.20);";

  // 面板是「常驻窗口 + 后端在弹出时发信号触发刷新」，浏览器里没有那条事件通道，
  // 这里补发一次 popover-shown，让预览的行为与真机（点图标弹出）一致 ——
  // 否则预览里面板永远停在「没被打开过」的状态，轮询也不会跑。
  //
  // 两个坑：一是这段必须留在 popover 分支里（放错分支就等于没执行）；
  // 二是重试不能用 rAF —— 一帧 ~16ms，40 帧窗口只有 0.6 秒，首屏渲染稍慢
  // 就把重试次数烧完，抢在监听器注册之前放弃了。
  let tries = 100;
  const announce = () => {
    if (window.__previewEmit?.("popover-shown")) return;
    if (--tries > 0) window.setTimeout(announce, 50);
  };
  window.setTimeout(announce, 50);
} else {
  const banner = document.createElement("div");
  banner.textContent = "浏览器预览模式 · 数据为演示内容，所有操作均不会真正执行";
  banner.style.cssText =
    "position:sticky;top:0;z-index:99;background:#fbf1dd;color:#96690f;" +
    "font:12px -apple-system,BlinkMacSystemFont,'PingFang SC',sans-serif;" +
    "padding:7px 20px;text-align:center;";
  document.body.prepend(banner);

  // 预览专用：?tab=ai / ?tab=ferry 时自动切到对应标签页，便于无头浏览器直接截图复核。
  const initialTab = previewParams.get("tab");
  const tabIndex = initialTab === "ai" ? 1 : initialTab === "ferry" ? 2 : 0;
  if (tabIndex > 0) {
    window.setTimeout(() => {
      const tabs = document.querySelectorAll<HTMLButtonElement>(".tabs .tab");
      tabs[tabIndex]?.click();
    }, 400);
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isPopoverPreview ? <PopoverApp /> : <App />}</React.StrictMode>,
);
