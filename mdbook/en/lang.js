// Language switch shared by both books: the English book is the site root, the Chinese one is under /zh-CN/.
// Adds a link in the menu bar to the same page in the other language.
(function () {
  'use strict';
  const ZH = '/zh-CN/';
  const path = window.location.pathname;
  const inZh = path.startsWith(ZH);
  const target = inZh ? '/' + path.slice(ZH.length) : ZH + path.replace(/^\//, '');
  const bar = document.querySelector('.right-buttons');
  if (!bar) return;
  const link = document.createElement('a');
  link.href = target + window.location.hash;
  link.title = inZh ? 'English' : '简体中文';
  link.setAttribute('aria-label', link.title);
  link.textContent = inZh ? 'EN' : '中文';
  link.style.cssText = 'font-weight:600;font-size:1.4rem;padding:0 0.5em;text-decoration:none;';
  bar.prepend(link);
})();
