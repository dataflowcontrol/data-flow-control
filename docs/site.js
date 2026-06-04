for (const button of document.querySelectorAll(".copy")) {
  button.addEventListener("click", async () => {
    const targetId = button.getAttribute("data-copy-target");
    const text = targetId
      ? document.getElementById(targetId).innerText
      : button.getAttribute("data-copy");
    await navigator.clipboard.writeText(text);
    const original = button.innerText;
    button.innerText = "Copied";
    window.setTimeout(() => {
      button.innerText = original;
    }, 1300);
  });
}
