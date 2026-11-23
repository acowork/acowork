<script data-acowork-bridge="2">
(function(){
  var APP="__APP__";
  var PAGE=(window.parent&&window.parent!==window)?window.parent:null;
  var TAGGED="data-acowork-canvas",observed=null,appReady=false;
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

  // The font stack lives in the app's wasm memory, so every frame reload starts
  // without it. Faces are fetched from this app's own origin and kept in the
  // page's own store, so a reopen of the same document re-adds them locally
  // instead of moving the bytes again.
  var FONT_STORE="acowork-fonts",fontDbPromise=null;

  function fontStore(mode,run){
    if(!fontDbPromise){
      if(typeof indexedDB==="undefined")return Promise.reject(new Error("no indexedDB"));
      fontDbPromise=new Promise(function(res,rej){
        try{
          var r=indexedDB.open(FONT_STORE,1);
          r.onupgradeneeded=function(){try{r.result.createObjectStore("fonts");}catch(e){}}
          r.onsuccess=function(){res(r.result);}
          r.onerror=function(){rej(r.error);}
        }catch(e){rej(e);}
      });
    }
    return fontDbPromise.then(function(db){
      return new Promise(function(res,rej){
        var tx=db.transaction("fonts",mode),req=run(tx.objectStore("fonts"));
        tx.oncomplete=function(){res(req&&req.result);}
        tx.onerror=tx.onabort=function(){rej(tx.error||new Error("font store failed"));}
      });
    });
  }

  function addFont(f){
    var a=api();
    if(!a||typeof a.execute!=="function"||!f||!f.data)return Promise.resolve(false);
    return Promise.resolve(a.execute("font.add",{data:f.data,name:f.name||"font"}))
      .then(function(){return true;},function(){return false;});
  }

  function remember(f){
    try{
      fontStore("readwrite",function(s){
        return s.put({family:f.family||f.name,name:f.name||"font",data:f.data},f.family||f.name||"font");
      }).catch(function(){});
    }catch(e){}
  }

  // A document names its typefaces and carries no glyphs (a PDF does, which is
  // why PdfCraft reads fine), so a deck in 宋体 draws tofu unless the app has the
  // face. The app reports what its document needs (`font.missing`) and this
  // same origin serves the machine's own copy at `/fonts/<family>` — the copy
  // the user already owns, never shipped in the package. The host is not
  // involved at all.
  function missingNames(r){
    var list=(r&&r.missing)||[],out=[];
    for(var i=0;i<list.length;i++){
      var v=list[i];
      var name=(typeof v==="string")?v:(v&&v.font)||"";
      if(name)out.push(name);
    }
    return out;
  }

  // `/fonts/<family>` of the app, wherever this page is mounted: ACowork serves the app under
  // `/api/apps/<id>/ui/` and proxies `/api/apps/<id>/fonts/*` to it, so a bare `/fonts` would
  // hit the Gateway root and 404. Opened directly, the base is empty.
  function fontUrl(family){
    var at=(location.pathname||"").indexOf("/ui/");
    var base=at>=0?(location.pathname||"").slice(0,at):"";
    return base+"/fonts/"+encodeURIComponent(family);
  }

  // A face that could not be fetched is the reason a document still draws tofu and still
  // says "isn't installed". Say so on the host's notice bar instead of failing silently:
  // the URL and the status are the whole diagnosis.
  var fontFails={};
  function fontFail(family,why){
    if(fontFails[family])return;
    fontFails[family]=1;
    post({channel:"acowork",type:"notify",message:"font "+family+": "+why});
  }

  // One family: the local copy first (a page reload keeps nothing in wasm, so
  // the bytes are re-added here), then this app's own `/fonts/<family>`.
  function loadFamily(family){
    return fontStore("readonly",function(s){return s.get(family);}).catch(function(){return null;})
      .then(function(row){
        if(row&&row.data)return addFont(row);
        var url=fontUrl(family);
        return fetch(url).then(function(r){
          if(!r.ok){fontFail(family,url+" -> HTTP "+r.status);return false;}
          return r.text().then(function(b64){
            var f={family:family,name:family,data:b64};
            return addFont(f).then(function(ok){
              if(ok)remember(f);else fontFail(family,"font.add refused "+family);
              return ok;
            });
          });
        }).catch(function(e){fontFail(family,url+" -> "+String(e&&e.message||e));return false;});
      });
  }

  function loadMissing(a,want){
    var i=0,loaded=0;
    return new Promise(function(res){
      (function next(){
        if(i>=want.length)return res(loaded);
        loadFamily(want[i++]).then(function(ok){if(ok)loaded++;}).then(next,next);
      })();
    });
  }

  // The fonts of the document just handed over. `done` always runs: a face this
  // machine does not have must never hold the document hostage.
  function settleFonts(a,done){
    if(!a||typeof a.execute!=="function")return done();
    var deadline=Date.now()+4000,quiet=0,QUIET_POLLS=3;
    function poll(){
      if(Date.now()>deadline)return done();
      Promise.resolve().then(function(){return a.execute("font.missing",{});})
        .then(function(r){
          // An app that says `ready:false` has not parsed the document yet, so
          // its empty list is "nothing to say", not "no fonts needed". An app
          // without the flag gets the benefit of the doubt after a second look.
          var undecided=(r&&r.ready===false)||(r&&r.ready===undefined&&!missingNames(r).length&&++quiet<QUIET_POLLS);
          if(undecided)return setTimeout(poll,100);
          var want=missingNames(r);
          if(!want.length)return done();
          quiet=0;
          return loadMissing(a,want).then(function(loaded){
            // Nothing loadable: a face this machine lacks is no reason to keep
            // the document waiting.
            if(loaded)return setTimeout(poll,0);
            return done();
          });
        })
        .catch(function(){done();}); // an app without `font.missing`
    }
    setTimeout(poll,80);
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
      el.addEventListener(APP+":ready",function(){appReady=true;flush();});
      el.addEventListener(APP+":save",function(e){
        var d=e.detail||{};
        // The app hands save bytes over as a transferred ArrayBuffer; the host writes a
        // Uint8Array (an ArrayBuffer has no `.length`, so base64 of it is empty — the file
        // lands on disk empty and reopens as "not a file this app can read").
        var b=d.bytes;
        var bytes=b?(b instanceof Uint8Array?b:new Uint8Array(b)):b;
        post({channel:"acowork",type:"saveFile",name:d.name||"",saveAs:!!d.saveAs,bytes:bytes,id:d.id});
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
      try{
        a.openFile(p.name,p.bytes);
        // Acknowledged only once the document's fonts are in, so the host keeps
        // its loader up and the file is painted once, with the right glyphs.
        settleFonts(a,function(){finish(p);});
        return;
      }catch(e){}
    }
    watch();
    if(!appReady&&p.tries<3){retry(p);return;}  // its host bridge has not announced itself yet
    // Sent anyway past the third try: an app that is up but missed the signal
    // still opens the document, and one that is not says `openFailed`.
    dispatchOpen(p);
    settleFonts(a,function(){finish(p);});
  }

  function finish(p){
    if(!p)return;
    if(pending===p)pending=null;
    if(opened===p.key)return;
    opened=p.key;
    post({channel:"acowork",type:"opened",id:p.id,name:p.name});
    // The document is on screen; the font menu still has only the bundled faces until the
    // ones a report or a slide most often picks are here. Load those now, in idle time, so
    // the visible document is never delayed by a face nobody asked for.
    setTimeout(preloadCommon,600);
  }

  // Common faces a Chinese document picks by hand. Names, not files: `/fonts` decides which
  // of them this machine has.
  var COMMON=["宋体","微软雅黑"];
  function preloadCommon(){
    var a=api();
    if(!a||typeof a.execute!=="function"||pending)return;
    var i=0;
    (function next(){
      if(i>=COMMON.length||pending)return;
      var name=COMMON[i++];
      var again=function(){setTimeout(next,50);};
      // Ask what the app can *draw*, not what this document lacks: a menu font is not in the
      // document, so `font.missing` answers "not missing" for exactly the ones wanted here.
      Promise.resolve()
        .then(function(){return a.execute("font.list",{all:true});})
        .then(function(r){
          var fams=(r&&r.families)||[];
          var have=fams.some(function(f){return String(f).toLowerCase()===name.toLowerCase();});
          return have?null:loadFamily(name);
        })
        .then(again,again);
    })();
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
    else if(d.type==="saved")fire("saved",{id:d.id});
    else if(d.type==="saveFailed")fire("failed",{error:d.error||"save failed",id:d.id});
  });

  hello();
})();
</script>