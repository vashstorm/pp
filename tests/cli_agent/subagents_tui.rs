// 使用真实 PTY 验证面板生命周期、取消和人工追问；消息始终按原文发送。
#[test]
fn subagent_tui_inspects_cancels_revives_and_sends_literal_followups(){
    for protocol in ["completion","response"]{
        let fixture=server();let home=tempfile::tempdir().unwrap();let project=tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(),"settings.yml"),fixture_settings()).unwrap();std::fs::write(config_path(home.path(),"models.yml"),response_models(&fixture,Some(protocol))).unwrap();
        let mut pty=Pty::start_with_args(project.path(),home.path(),&[]);pty.ready();
        pty.input("/agents\r");pty.text("scout");pty.text("bundled");pty.input("\x1b");pty.ready();
        pty.input("delegate terminal task\r");let(request,_)=pty.request_at(&fixture,api_path(protocol));
        let task=json!({"context":"terminal context","tasks":[{"task":"child terminal work"}]});
        let call=if protocol=="completion"{call("task","task",task)}else{response_call("task","task",task)};
        ask_reply_calls(request,protocol,json!([call]));
        let mut child=None;let mut parent=false;
        while child.is_none()||!parent{
            let(request,body)=pty.request_at(&fixture,api_path(protocol));let system=if protocol=="completion"{body["messages"][0].to_string()}else{body["instructions"].to_string()};
            if system.contains("You are subagent"){child=Some(request);}else{ask_final_reply(request,protocol,"Waiting for subagent.");parent=true;}
        }
        pty.input("/subagents\r");pty.text("Subagents");pty.text("a1");pty.input("c");
        let(request,body)=pty.request_at(&fixture,api_path(protocol));assert!(body.to_string().contains("cancelled"));drop(child.take());
        pty.input("\x1b");ask_final_reply(request,protocol,"child-cancelled");pty.text("child-cancelled");pty.ready();
        pty.input("/subagents\r");pty.text("a1");pty.input("r");pty.input("e/model hidden\r");
        let(request,body)=pty.request_at(&fixture,api_path(protocol));assert!(body.to_string().contains("You are subagent"));assert!(body.to_string().contains("/model hidden"));assert_eq!(body["model"],"fixture-model");
        ask_final_reply(request,protocol,"child-followup-answer");
        let(request,body)=pty.request_at(&fixture,api_path(protocol));assert!(body.to_string().contains("child-followup-answer"));ask_final_reply(request,protocol,"parent-followup-answer");
        pty.input("\x1b");pty.text("parent-followup-answer");pty.ready();pty.input("/subagents\r");pty.text("a1");pty.input("x");pty.text("aborted");pty.input("\x1b\x03");pty.finish();
    }
}
