//! The `service!` proc-macro: from one service definition, generate the
//! request/response enums, a typed client, and a dispatch impl — kept as its
//! own crate because Rust forbids proc-macros in a lib crate.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{parse_macro_input, FnArg, ItemTrait, Pat, PatType, ReturnType, TraitItem, Type};

#[proc_macro]
pub fn service(input: TokenStream) -> TokenStream {
	let item: ItemTrait = parse_macro_input!(input as ItemTrait);
	expand(item).unwrap_or_else(|e| e.to_compile_error().into())
}

struct Method {
	name: syn::Ident,
	args: Vec<(syn::Ident, Type)>,
	ret: Type,
}

fn expand(input: ItemTrait) -> syn::Result<TokenStream> {
	let trait_name = input.ident.clone();
	let trait_vis = input.vis.clone();
	let snake = to_snake(&trait_name.to_string());
	let client_ident = format_ident!("{}Client", trait_name);
	let serve_ident = format_ident!("serve_{}", snake);

	let mut methods = Vec::new();
	let mut trait_items = Vec::new();

	for item in &input.items {
		match item {
			TraitItem::Fn(m) => {
				if m.sig.asyncness.is_none() {
					return Err(syn::Error::new_spanned(
						&m.sig,
						"service! methods must be `async fn`",
					));
				}
				let name = m.sig.ident.clone();
				let mut args: Vec<(syn::Ident, Type)> = Vec::new();
				for input in &m.sig.inputs {
					match input {
						FnArg::Receiver(r) => {
							return Err(syn::Error::new_spanned(
								r,
								"service! methods must not take a receiver",
							));
						}
						FnArg::Typed(PatType { pat, ty, .. }) => {
							let ident = match pat.as_ref() {
								Pat::Ident(p) => p.ident.clone(),
								_ => {
									return Err(syn::Error::new_spanned(
										pat,
										"service! method parameters must be plain identifiers",
									));
								}
							};
							args.push((ident, (**ty).clone()));
						}
					}
				}
				let ret: Type = match &m.sig.output {
					ReturnType::Default => syn::parse_quote!(()),
					ReturnType::Type(_, t) => (**t).clone(),
				};

				let arg_pats = args.iter().map(|(n, t)| quote! { #n: #t });
				trait_items.push(quote! {
						fn #name(&self, #( #arg_pats ),*)
								-> impl ::core::future::Future<Output = #ret> + ::core::marker::Send;
				});

				methods.push(Method { name, args, ret });
			}
			other => {
				trait_items.push(quote! { #other });
			}
		}
	}

	let trait_attrs = &input.attrs;
	let trait_decl = quote! {
			#( #trait_attrs )*
			#trait_vis trait #trait_name: ::core::marker::Send + ::core::marker::Sync + 'static {
					#( #trait_items )*
			}
	};

	let client_methods = methods.iter().map(client_method);
	let server_arms = methods.iter().map(|m| server_arm(&trait_name, m));

	let codec_bound = quote! {
			::transport::typed::Codec<Frame = ::transport::__private::serde_json::Value>
					+ ::core::default::Default
					+ ::transport::__private::tokio_util::codec::Encoder<
							::transport::__private::serde_json::Value,
							Error = ::transport::typed::CodecError,
					>
					+ ::transport::__private::tokio_util::codec::Decoder<
							Item = ::transport::__private::serde_json::Value,
							Error = ::transport::typed::CodecError,
					>
	};

	let client_decl = quote! {
			#trait_vis struct #client_ident<C>
			where
					C: #codec_bound,
			{
					channel: ::transport::__private::tokio::sync::Mutex<::transport::typed::Channel<C>>,
					next_id: ::core::sync::atomic::AtomicU64,
					pending: ::transport::__private::tokio::sync::Mutex<
							::std::collections::HashMap<u64, ::transport::__private::serde_json::Value>,
					>,
			}

			impl<C> #client_ident<C>
			where
					C: #codec_bound,
			{
					pub fn new(channel: ::transport::typed::Channel<C>) -> Self {
							Self {
									channel: ::transport::__private::tokio::sync::Mutex::new(channel),
									next_id: ::core::sync::atomic::AtomicU64::new(1),
									pending: ::transport::__private::tokio::sync::Mutex::new(
											::std::collections::HashMap::new(),
									),
							}
					}

					async fn __call(
							&self,
							method: &'static str,
							params: ::transport::__private::serde_json::Value,
					) -> ::core::result::Result<
							::transport::__private::serde_json::Value,
							::transport::typed::RpcError,
					> {
							use ::core::sync::atomic::Ordering;
							let id = self.next_id.fetch_add(1, Ordering::Relaxed);
							let envelope = ::transport::__private::serde_json::json!({
									"id": id,
									"method": method,
									"params": params,
							});
							{
									let mut chan = self.channel.lock().await;
									chan.send(envelope).await
											.map_err(|e| ::transport::typed::RpcError::Adapter(e.to_string()))?;
							}
							loop {
									{
											let mut pend = self.pending.lock().await;
											if let Some(v) = pend.remove(&id) {
													return Self::__decode_reply(v);
											}
									}
									let frame = {
											let mut chan = self.channel.lock().await;
											chan.recv().await
													.map_err(|e| ::transport::typed::RpcError::Adapter(e.to_string()))?
													.ok_or_else(|| ::transport::typed::RpcError::Adapter("eof".into()))?
									};
									let rid = frame.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
									if rid == id {
											return Self::__decode_reply(frame);
									} else {
											let mut pend = self.pending.lock().await;
											pend.insert(rid, frame);
									}
							}
					}

					fn __decode_reply(
							frame: ::transport::__private::serde_json::Value,
					) -> ::core::result::Result<
							::transport::__private::serde_json::Value,
							::transport::typed::RpcError,
					> {
							if let Some(err) = frame.get("error") {
									let msg = err.get("message")
											.and_then(|v| v.as_str())
											.unwrap_or("rpc error")
											.to_string();
									return Err(::transport::typed::RpcError::Application(msg));
							}
							Ok(frame.get("result").cloned().unwrap_or(
									::transport::__private::serde_json::Value::Null,
							))
					}

					#( #client_methods )*
			}
	};

	let serve_decl = quote! {
			#trait_vis async fn #serve_ident<C, H>(
					mut channel: ::transport::typed::Channel<C>,
					handler: H,
			) -> ::core::result::Result<(), ::transport::typed::AdapterError>
			where
					C: #codec_bound,
					H: #trait_name,
			{
					loop {
							let frame = match channel.recv().await? {
									Some(f) => f,
									None => return Ok(()),
							};
							let id = frame.get("id").cloned().unwrap_or(
									::transport::__private::serde_json::Value::Null,
							);
							let method = frame.get("method")
									.and_then(|v| v.as_str())
									.unwrap_or("")
									.to_string();
							let params = frame.get("params").cloned().unwrap_or(
									::transport::__private::serde_json::Value::Null,
							);
							let reply = match method.as_str() {
									#( #server_arms )*
									other => ::transport::__private::serde_json::json!({
											"id": id,
											"error": { "code": -32601, "message": format!("method not found: {}", other) }
									}),
							};
							channel.send(reply).await?;
					}
			}
	};

	let out = quote! {
			#trait_decl
			#client_decl
			#serve_decl
	};
	Ok(out.into())
}

fn client_method(m: &Method) -> TokenStream2 {
	let name = &m.name;
	let method_str = name.to_string();
	let ret = &m.ret;
	let arg_decls = m.args.iter().map(|(n, t)| quote! { #n: #t });
	let arg_inserts = m.args.iter().map(|(n, _)| {
		let key = n.to_string();
		quote! {
				params_obj.insert(
						#key.to_string(),
						::transport::__private::serde_json::to_value(&#n)
								.map_err(|e| ::transport::typed::RpcError::Codec(e.to_string()))?,
				);
		}
	});
	quote! {
			pub async fn #name(
					&self,
					#( #arg_decls ),*
			) -> ::core::result::Result<#ret, ::transport::typed::RpcError> {
					let mut params_obj = ::transport::__private::serde_json::Map::new();
					#( #arg_inserts )*
					let params = ::transport::__private::serde_json::Value::Object(params_obj);
					let result = self.__call(#method_str, params).await?;
					::transport::__private::serde_json::from_value::<#ret>(result)
							.map_err(|e| ::transport::typed::RpcError::Codec(e.to_string()))
			}
	}
}

fn server_arm(_trait_name: &syn::Ident, m: &Method) -> TokenStream2 {
	let name = &m.name;
	let method_str = name.to_string();
	let arg_decodes = m.args.iter().map(|(n, t)| {
		let key = n.to_string();
		quote! {
				let #n: #t = match params.get(#key).cloned() {
						Some(v) => match ::transport::__private::serde_json::from_value::<#t>(v) {
								Ok(x) => x,
								Err(e) => {
										let reply = ::transport::__private::serde_json::json!({
												"id": id,
												"error": { "code": -32602, "message": format!("decode {}: {}", #key, e) }
										});
										channel.send(reply).await?;
										continue;
								}
						},
						None => {
								let reply = ::transport::__private::serde_json::json!({
										"id": id,
										"error": { "code": -32602, "message": format!("missing param: {}", #key) }
								});
								channel.send(reply).await?;
								continue;
						}
				};
		}
	});
	let arg_names = m.args.iter().map(|(n, _)| quote! { #n });
	quote! {
			#method_str => {
					#( #arg_decodes )*
					let out = handler.#name( #( #arg_names ),* ).await;
					match ::transport::__private::serde_json::to_value(&out) {
							Ok(v) => ::transport::__private::serde_json::json!({ "id": id, "result": v }),
							Err(e) => ::transport::__private::serde_json::json!({
									"id": id,
									"error": { "code": -32603, "message": format!("encode result: {}", e) }
							}),
					}
			}
	}
}

fn to_snake(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 4);
	for (i, ch) in s.chars().enumerate() {
		if ch.is_ascii_uppercase() {
			if i != 0 {
				out.push('_');
			}
			out.push(ch.to_ascii_lowercase());
		} else {
			out.push(ch);
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::to_snake;

	#[test]
	fn to_snake_lowercases_and_word_splits_on_interior_capitals() {
		assert_eq!(to_snake("MemoryRpc"), "memory_rpc");
		assert_eq!(to_snake("KernRpc"), "kern_rpc");
		assert_eq!(to_snake("SearchSvc"), "search_svc");
		assert_eq!(to_snake("Memory"), "memory");
		assert_eq!(to_snake("X"), "x");
		assert_eq!(to_snake("ABC"), "a_b_c");
		assert_eq!(to_snake("already_snake"), "already_snake");
		assert_eq!(to_snake(""), "");
	}
}
