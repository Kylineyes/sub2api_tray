//! 演示夹具回归测试。
//!
//! 这些断言保证 `testdata/demo` 始终是同一套演示账号与展示数据，
//! 后续补充字段时只扩展夹具，不要让截图与测试各自漂移。

#[test]
fn 演示账号与展示数据保持固定() {
    let accounts: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/demo/accounts.template.json")).unwrap();
    let account = &accounts["accounts"][0];
    assert_eq!(account["provider"], "sub2Api");
    assert_eq!(account["email"], "demo@example.com");
    assert_eq!(account["baseUrl"], "http://127.0.0.1:18080");
    assert_eq!(account["apiKeys"].as_array().unwrap().len(), 2);

    let keys: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/demo/api/keys.json")).unwrap();
    assert_eq!(keys["total"], 2);
    assert_eq!(keys["items"][0]["name"], "Demo Key A");
    assert_eq!(keys["items"][0]["quota"], 100.0);
    assert_eq!(keys["items"][0]["quota_used"], 12.3456);

    let usage: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/demo/api/usage.json")).unwrap();
    assert_eq!(usage["trend"].as_array().unwrap().len(), 5);
    assert_eq!(usage["granularity"], "hour");

    let deepseek: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/demo/api/deepseek_balance.json")).unwrap();
    assert_eq!(deepseek["balance_infos"][0]["currency"], "CNY");
    assert_eq!(deepseek["balance_infos"][0]["total_balance"], "88.88");
}
