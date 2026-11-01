(() => {
  "use strict";
  const form = document.querySelector("#login-form");
  const submit = document.querySelector("#submit");
  const notice = document.querySelector("#notice");
  let enrollment = false;
  async function status() {
    try {
      const response = await fetch("/auth/status", { cache: "no-store", credentials: "same-origin" });
      const value = await response.json();
      if (!response.ok) throw new Error(value.error || "无法读取管理状态。");
      enrollment = value.enrollmentRequired;
      document.querySelector("#pairing-field").hidden = !enrollment;
      document.querySelector("#confirmation-field").hidden = !enrollment;
      form.elements.pairingCode.required = enrollment;
      form.elements.confirmation.required = enrollment;
      if (enrollment) form.elements.password.autocomplete = "new-password";
      else form.elements.password.autocomplete = "current-password";
      document.querySelector("#title").textContent = enrollment ? "设置管理密码" : "登录管理台";
      submit.textContent = enrollment ? "完成配对并设置密码" : "登录";
      submit.disabled = value.enrollmentExpired;
      if (value.enrollmentExpired) notice.textContent = "配对码已过期。请让部署 Agent 重启管理服务，然后由本人读取新的配对文件。";
    } catch (error) { notice.textContent = error.message; }
  }
  form.addEventListener("submit", async event => {
    event.preventDefault();
    notice.textContent = "";
    const password = form.elements.password.value;
    if (enrollment && password !== form.elements.confirmation.value) {
      notice.textContent = "两次输入的密码不一致。";
      return;
    }
    submit.disabled = true;
    const body = enrollment ? { password, pairingCode: form.elements.pairingCode.value.trim() } : { password };
    try {
      const response = await fetch(enrollment ? "/auth/enroll" : "/auth/login", {
        method: "POST", credentials: "same-origin", cache: "no-store",
        headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
      });
      const value = await response.json();
      if (!response.ok) throw new Error(value.error || "登录未完成。");
      form.reset();
      window.location.replace("/");
    } catch (error) {
      notice.textContent = error.message;
      submit.disabled = false;
    }
  });
  status();
})();
