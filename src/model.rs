#![allow(dead_code)]

pub mod observer;
pub mod util;

use crate::futures::StreamExt;
use crate::macros::{error, trace};
use crate::model::observer::Observer;
use crate::model::util::ModelTimestamps;
use crate::Spark;
use mongodb::bson::{doc, to_document, Document};
use mongodb::error::Result;
use mongodb::options::{
	DeleteOptions, DropIndexOptions, FindOneOptions, FindOptions, InsertOneOptions,
	ListIndexesOptions, UpdateOptions,
};
use mongodb::results::UpdateResult;
use mongodb::{Collection, Cursor, Database, IndexModel};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Duration;

// TODO: this must move to types module
type Id = mongodb::bson::Bson;
pub type MongodbResult<T> = Result<T>;

const HEAP_THRESHOLD: usize = 256;

#[derive(Serialize, Debug)]
pub enum Inner<M> {
	Stack(M),
	Heap(Box<M>),
}

#[derive(Debug, Serialize)]
pub struct Model<'a, M> {
	inner: Inner<M>,
	#[serde(skip)]
	db: Arc<Database>,
	#[serde(skip)]
	collection_name: &'a str,
	#[serde(skip)]
	collection: Collection<M>,
}

impl<M> Deref for Inner<M> {
	type Target = M;
	fn deref(&self) -> &Self::Target {
		match &self {
			Inner::Heap(t) => t,
			Inner::Stack(t) => t,
		}
	}
}

impl<M> DerefMut for Inner<M> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		match self {
			Inner::Heap(t) => t,
			Inner::Stack(t) => t,
		}
	}
}

impl<'a, T: 'a> Deref for Model<'a, T> {
	type Target = T;

	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl<'a, T: 'a> DerefMut for Model<'a, T> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.inner
	}
}

impl<'a, M> Model<'a, M>
where
	M: Default,
	M: Serialize,
	M: DeserializeOwned,
	M: Send,
	M: Sync,
	M: Unpin,
	M: Debug,
	M: ModelTimestamps,
	M: Observer<M>,
{
	/// makes a model and stores the data and collection_name to creating collection object
	/// to store data into it
	///
	/// # Arguments
	///
	/// * `db`: you cna pass None , in this way model created by global spark connection , or you can pass your own database
	/// * `collection_name`:  it's collection name that we use in create collection object
	///
	/// returns: Model<M>
	///
	/// # Examples
	///
	/// ```
	/// struct User{
	///     name: String
	/// }
	/// let db = ...;
	/// let user_model = Model::<User>::new(Arc::clone(db) , "users");
	/// ```
	pub fn new(db: Option<&Arc<Database>>, collection_name: &'a str) -> Model<'a, M> {
		let inner = if std::mem::size_of::<M>() > 250 {
			Inner::Heap(Box::<M>::default())
		} else {
			Inner::Stack(M::default())
		};

		if let Some(database) = db {
			let collection = database.collection::<M>(collection_name);
			return Model {
				inner,
				// inner: Box::<M>::default(),
				db: database.clone(),
				collection_name,
				collection,
			};
		}
		// it panics if it's not initialized before use
		let database = Spark::get_db();
		let collection = database.collection::<M>(collection_name);
		Model {
			inner,
			db: database,
			collection_name,
			collection,
		}
	}

	/// saves the change , if the inner has some _id then it's update the existing unless
	/// it's create  new document
	pub async fn save(
		&mut self,
		options: impl Into<Option<InsertOneOptions>>,
	) -> MongodbResult<Id> {
		self.inner.updated_at();
		let mut converted = to_document(&self.inner)?;
		if let Some(id) = converted.get("_id") {
			let owned_id = id.to_owned();
			let upsert = self
				.collection
				.update_one(
					doc! {
						"_id" : id
					},
					doc! { "$set": &converted},
					None,
				)
				.await?;
			if upsert.modified_count >= 1 {
				// dispatch call
				// this must be pinned to handle recursive async call
				Box::pin(M::updated(self)).await?;

				return Ok(owned_id);
			};
		}
		converted.remove("_id");
		self.inner.created_at();

		let re = self.collection.insert_one(&*self.inner, options).await?;

		// dispatch observer
		// this must be pinned to handle recursive async call
		Box::pin(M::created(self)).await?;

		Ok(re.inserted_id)
	}
	pub async fn find_one(
		&mut self,
		doc: impl Into<Document>,
		options: impl Into<Option<FindOneOptions>>,
	) -> MongodbResult<Option<&mut Self>> {
		let result = self.collection.find_one(Some(doc.into()), options).await?;
		match result {
			Some(inner) => {
				self.fill(inner);
				Ok(Some(self))
			}
			None => Ok(None),
		}
	}

	/// this is raw update , and you can pass document or your model
	/// # Examples
	/// ## with the raw doc
	///  ```
	///  let user_model = User::new_model(Some(&db));
	///     let updated = user_model.update(
	///         doc! {
	///             "name": "Hossein",
	///         },
	///         doc! {
	///            "$set": {
	///                 "name": "Hossein 33"
	///             }
	///         },
	///         None,
	///     ).await.unwrap();
	/// ```
	/// ## with the model
	/// let user_model = User::new_model(Some(&db));
	///     let mut sample_user = User::default();
	///     sample_user.name = "Hossein 33".to_string();
	///     let updated = user_model.update(
	///         &sample_user,
	///        doc! {
	///            "$set": {
	///                "name": "Hossein 3355"
	///            }
	///        },
	///        None,
	///    ).await.unwrap();
	///
	/// ## with_model_instance
	///     let mut user_model = User::new_model(Some(&db));
	///    user_model.name = "Hossein 3355".to_string();
	///    user_model.age = 58;
	///    let updated = user_model.update(
	///        &user_model,
	///        doc! {
	///            "$set": {
	///                "name": "Hossein 325"
	///            }
	///        },
	///        None,
	///    ).await.unwrap();
	///
	/// NOTE : updated observer doesn't execute in this method
	///
	pub async fn update(
		&self,
		query: impl Into<Document>,
		doc: impl Into<Document>,
		options: impl Into<Option<UpdateOptions>>,
	) -> MongodbResult<UpdateResult> {
		self.collection.update_one(query.into(), doc.into(), options).await
	}

	pub async fn find(
		&self,
		filter: impl Into<Document>,
		options: impl Into<Option<FindOptions>>,
	) -> MongodbResult<Cursor<M>> {
		self.collection.find(Some(filter.into()), options).await
	}

	pub async fn find_and_collect(
		&self,
		filter: impl Into<Document>,
		options: impl Into<Option<FindOptions>>,
	) -> MongodbResult<Vec<MongodbResult<M>>> {
		// TODO write this in other functions
		let converted = filter.into();
		let doc = if converted.is_empty() {
			None
		} else {
			Some(converted)
		};

		let future = self.collection.find(doc, options).await?;
		Ok(future.collect().await)
	}

	pub fn register_attributes(&self, attributes: Vec<&str>) {
		let mut attrs = attributes.iter().map(|attr| attr.to_string()).collect::<Vec<String>>();
		let max_time_to_drop = Some(Duration::from_secs(5));
		let (tx, _) = tokio::sync::oneshot::channel();
		let db = self.db.clone();
		let coll_name = self.collection_name.to_owned();
		trace!("Spawn task to register indexes");
		let register_attrs = async move {
			let coll = db.collection::<M>(&coll_name);
			let previous_indexes = coll
				.list_indexes(Some(
					ListIndexesOptions::builder().max_time(max_time_to_drop).build(),
				))
				.await;

			let mut keys_to_remove = Vec::new();

			if previous_indexes.is_ok() {
				let foreach_future = previous_indexes.unwrap().for_each(|pr| {
					match pr {
						Ok(index_model) => {
							index_model.keys.iter().for_each(|key| {
								if key.0 != "_id" {
									if let Some(pos) = attrs.iter().position(|k| k == key.0) {
										// means attribute exists in struct and database and not need to create it
										attrs.remove(pos);
									} else if let Some(rw) = &index_model.options {
										// means the attribute must remove because not exists in struct
										keys_to_remove.push(rw.name.clone())
									}
								}
							});
						}
						Err(error) => {
							error!("Can't unpack index model {error}");
						}
					}
					futures::future::ready(())
				});
				foreach_future.await;
			}
			let attrs = attrs
				.iter()
				.map(|attr| {
					let key = attr.to_string();
					IndexModel::builder()
						.keys(doc! {
							key : 1
						})
						.build()
				})
				.collect::<Vec<IndexModel>>();

			for name in keys_to_remove {
				let key = name.as_ref().unwrap();
				let _ = coll
					.drop_index(
						key,
						Some(DropIndexOptions::builder().max_time(max_time_to_drop).build()),
					)
					.await;
			}
			if !attrs.is_empty() {
				let result = coll.create_indexes(attrs, None).await;
				if let Err(error) = result {
					error!("Can't create indexes : {:?}", error);
				}
			}
		};

		//TODO remove this thread , I think the channel is useless, review them
		let task = tokio::spawn(register_attrs);

		let wait_for_complete = async move {
			let _ = task.await;
			let _ = tx.send(());
		};

		tokio::task::spawn(wait_for_complete);
	}

	pub async fn delete(
		&mut self,
		query: impl Into<Document>,
		options: impl Into<Option<DeleteOptions>>,
	) -> MongodbResult<u64> {
		let re = self.collection.delete_one(query.into(), options).await?.deleted_count;

		// dispatch observer
		// this must be pinned to handle recursive async call
		M::deleted(self).await?;

		Ok(re)
	}

	pub fn fill(&mut self, inner: M) {
		*self.inner = inner;
	}
}

impl<'a, M> Model<'a, M>
where
	M: Default,
	M: Serialize,
{
	/// this method takes the inner and gives you ownership of inner then
	/// replace it with default value
	pub fn take_inner(&mut self) -> M {
		std::mem::take(&mut *self.inner)
	}

	pub fn inner_ref(&self) -> &M {
		&self.inner
	}

	pub fn inner_mut(&mut self) -> &mut M {
		&mut self.inner
	}

	pub fn inner_to_doc(&self) -> MongodbResult<Document> {
		let re = to_document(&self.inner)?;
		Ok(re)
	}
}

// converts

impl<'a, M> From<Model<'a, M>> for Document
where
	M: Serialize,
{
	fn from(value: Model<M>) -> Self {
		mongodb::bson::to_document(&value.inner).unwrap()
	}
}

impl<'a, M> From<&Model<'a, M>> for Document
where
	M: Serialize,
{
	fn from(value: &Model<'a, M>) -> Self {
		mongodb::bson::to_document(&value.inner).unwrap()
	}
}

impl<'a, M> From<&mut Model<'a, M>> for Document
where
	M: Serialize,
{
	fn from(value: &mut Model<'a, M>) -> Self {
		mongodb::bson::to_document(&value.inner).unwrap()
	}
}
