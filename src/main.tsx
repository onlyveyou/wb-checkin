import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import PopoverApp from "./PopoverApp";
import "./styles.css";
// 主题层放在最后，稳定覆盖基础样式与旧的精修层
import "./theme-v3.css";

// 主窗口与菜单栏面板共用这一份 bundle，靠窗口 label 分流。
// 好处是构建配置不用开多入口，面板与主界面也天然共享类型和接口层。
const isPopover = getCurrentWindow().label === "popover";

// 面板窗口是透明窗口，body 不能铺底色，否则圆角外会露出方角
if (isPopover) document.body.classList.add("popover-body");

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isPopover ? <PopoverApp /> : <App />}</React.StrictMode>,
);
