
(function(){
  var APP="__APP__";
  var PAGE=(window.parent&&window.parent!==window)?window.parent:null;
  var TAGGED="data-acowork-canvas",observed=null,appReady=false,fontsAsked=false;
  var pending=null,opened=null,lastDark=null,applied=null,themeTimer=null,seen={};
  var MAX_TRIES=6;

  function post(m){try{if(PAGE)PAGE.postMessage(m,"*");}catch(e){}}
  function api(){try{return window[APP]||null;}catch(e){return null;}}
  function stopTimer(){if(themeTimer){clearInterval(themeTimer);themeTimer=null;}}

  // The host learns the app is up from `hello` (bridge 2), and an old host from
  // `ready`. Both, because a package outlives the Desktop build hosting it.
  function hello(){
    post({channel:"acowork",type:"hello",bridge:2,app:APP});
    post({channel:"acowork",type:"ready"});
  }

  // Anything the app throws reaches the host's log: without it a user's "it
  // hangs" is a repro hunt with no evidence.
  window.addEventListener("error",function(e){
    post({channel:"acowork",type:"error",message:String((e&&e.message)||"script error")});
  });
  window.addEventListener("unhandledrejection",function(e){
    post({channel:"acowork",type:"error",message:String((e&&e.reason)||"unhandled rejection")});
  });

  // The app's own light/dark command, over its page API.
  function themeCommand(dark){__THEME__}

  function applyTheme(){
    if(lastDark===null)return;
    document.documentElement.style.colorScheme=lastDark?"dark":"light";
    if(applied===lastDark){stopTimer();return;}
    var c=themeCommand(lastDark),a=api();
    if(!c){applied=lastDark;stopTimer();return;}
    try{
      if(a&&typeof a.execute==="function"){a.execute(c[0],c[1]);applied=lastDark;}
      else if(a&&typeof a.command==="function"){a.command(c[0],JSON.stringify(c[1]));applied=lastDark;}
    }catch(e){}
    if(applied===lastDark)stopTimer();
  }
  function startTheme(){applyTheme();if(applied===null&&!themeTimer)themeTimer=setInterval(applyTheme,200);}

  // A document names the fonts it uses and embeds no glyphs (a PDF does, which
  // is why PdfCraft reads fine), so `.docx` text in 宋体 draws tofu unless the
  // font is installed. An app that can list and add fonts asks the host for the
  // ones it lacks; one that cannot never asks, and the host injects nothing.
  function syncFonts(){
    var a=api();
    if(fontsAsked||!a||typeof a.execute!=="function")return;
    fontsAsked=true;
    Promise.resolve(a.execute("font.list",{all:true})).then(function(r){
      post({channel:"acowork",type:"fontsWanted",have:(r&&r.families)||[]});
    }).catch(function(){});
  }
  // One at a time: each add relayouts the document and a font file is tens of
  // megabytes, so firing them together stalls the frame.
  function installFonts(list){
    var a=api();
    if(!a||typeof a.execute!=="function"||!list||!list.length)return;
    var i=0;
    (function next(){
      if(i>=list.length)return;
      var f=list[i++];
      if(!f||!f.data){next();return;}
      Promise.resolve(a.execute("font.add",{data:f.data,name:f.name||"font"}))
        .catch(function(){})
        .then(function(){setTimeout(next,0);});
    })();
  }

  // Every canvas the app draws on: `data-host="page"` turns on the app's own
  // host bridge on that element, and the app reads it once at startup, so it is
  // set on any canvas as soon as one appears.
  function canvases(){
    var out=[],list;
    try{list=document.getElementsByTagName("canvas");}catch(e){return out;}
    for(var i=0;i<list.length;i++){
      if(!list[i].hasAttribute("data-host"))list[i].setAttribute("data-host","page");
      out.push(list[i]);
    }
    return out;
  }

  // Listen on each canvas once. `<app>:ready` is the app's "host bridge is
  // listening" signal and it does not bubble, so it is attached per canvas.
  function watch(){
    var list=canvases();
    for(var i=0;i<list.length;i++){
      var el=list[i];
      if(el.hasAttribute(TAGGED))continue;
      el.setAttribute(TAGGED,"1");
      el.addEventListener(APP+":ready",function(){appReady=true;flush();syncFonts();});
      el.addEventListener(APP+":save",function(e){
        var d=e.detail||{};
        post({channel:"acowork",type:"saveFile",name:d.name||"",saveAs:!!d.saveAs,bytes:d.bytes,id:d.id});
      });
      el.addEventListener(APP+":failed",function(e){
        post({channel:"acowork",type:"notify",message:String((e.detail||{}).error||"the app reported a failure")});
      });
      captureDownloads();
      startTheme();
    }
  }

__CAPTURE__

  function fire(kind,detail){
    var list=canvases();
    for(var i=0;i<list.length;i++){
      try{list[i].dispatchEvent(new CustomEvent(APP+":"+kind,{detail:detail||{}}));}catch(e){}
    }
  }

  // A workspace file reaches the app as bytes: through its page API when that
  // takes bytes (VectorCraft), else as its host bridge's open message. Delivery
  // is reported back (`opened` / `openFailed`) so the host never sits on a
  // loader waiting for something that already went wrong.
  function deliver(d){
    var b=d.bytes;
    if(!b)return;
    var bytes=(b instanceof Uint8Array)?b:new Uint8Array(b);
    var name=d.name||"document";
    pending={name:name,bytes:bytes,id:d.id,key:name+":"+bytes.length,tries:0};
    attempt(pending);
  }

  function dispatchOpen(p){
    var list=canvases();
    for(var i=0;i<list.length;i++){
      try{list[i].dispatchEvent(new CustomEvent(APP+":open",{detail:{name:p.name,bytes:p.bytes}}));}catch(e){}
    }
  }

  function attempt(p){
    if(!p||pending!==p)return;
    var a=api();
    if(a&&typeof a.openFile==="function"){
      try{a.openFile(p.name,p.bytes);finish(p);return;}catch(e){}
    }
    watch();
    if(!appReady&&p.tries<3){retry(p);return;}  // its host bridge has not announced itself yet
    // Sent anyway past the third try: an app that is up but missed the signal
    // still opens the document, and one that is not says `openFailed`.
    dispatchOpen(p);
    finish(p);
  }

  function finish(p){
    if(!p)return;
    if(pending===p)pending=null;
    if(opened===p.key)return;
    opened=p.key;
    post({channel:"acowork",type:"opened",id:p.id,name:p.name});
  }

  function retry(p){
    if(!p||pending!==p)return;
    p.tries++;
    if(p.tries>=MAX_TRIES){
      pending=null;
      post({channel:"acowork",type:"openFailed",id:p.id,error:"the app did not start in time, so the document was not handed over"});
      return;
    }
    setTimeout(function(){attempt(p);},1000);
  }

  // A document that arrives before the app is up waits here; the first canvas
  // flushes it.
  function flush(){if(pending)attempt(pending);}

  try{
    observed=new MutationObserver(function(){watch();});
    observed.observe(document.documentElement,{childList:true,subtree:true});
  }catch(e){}
  watch();

  window.addEventListener("message",function(e){
    var d=e.data;
    if(!d||d.channel!=="acowork")return;
    if(d.type==="theme"){lastDark=(d.theme==="dark");applied=null;startTheme();}
    else if(d.type==="openFile")deliver(d);
    else if(d.type==="fonts")installFonts(d.fonts);
    else if(d.type==="saved")fire("saved",{id:d.id});
    else if(d.type==="saveFailed")fire("failed",{error:d.error||"save failed",id:d.id});
  });

  hello();
})();
