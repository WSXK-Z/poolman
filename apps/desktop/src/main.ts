// Vue 桌面应用入口：注册 Naive UI 组件并挂载根视图。
import { createApp } from "vue";
import App from "./App.vue";
import naive from "naive-ui";
import "./styles/design-system.css";

createApp(App).use(naive).mount("#app");
